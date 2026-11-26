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

//! Joining a meeting through meeting-api and minting room tokens.
//!
//! Flow: `GET /meetings/{id}` (the CLI never creates meetings) →
//! `POST /meetings/{id}/join` → wait in the waiting room if needed →
//! `GET /meetings/{id}/status` for a fresh room token on every initial
//! relay connect attempt. Once connected, a dropped relay session ends the
//! stream; the CLI does not reconnect.

use secrecy::SecretString;
use videocall_meeting_client::ApiError;
use videocall_meeting_types::responses::ParticipantStatusResponse;

use crate::auth::{AuthError, Credentials};
use std::time::{Duration, Instant};

use crate::backoff::{poll_delay, Backoff};

/// Why joining or minting a room token failed.
#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    /// Login, token, or transport problem talking to meeting-api.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The meeting doesn't exist.
    #[error(
        "meeting '{0}' doesn't exist. Create it in a browser first; the CLI can only join \
         existing meetings."
    )]
    MeetingNotFound(String),
    /// The host rejected the join request.
    #[error("the host declined your request to join meeting '{0}'")]
    Rejected(String),
    /// The meeting ended.
    #[error("meeting '{0}' has ended")]
    MeetingEnded(String),
    /// meeting-api says we're no longer admitted.
    #[error("you are not admitted to meeting '{0}' (status: {1})")]
    NotAdmitted(String, String),
    /// Admitted but the response carried no room token.
    #[error("meeting API said you're admitted but sent no room token")]
    MissingRoomToken,
}

impl RoomError {
    /// True for failures worth retrying (network, timeouts, 5xx, 429).
    pub fn is_retryable(&self) -> bool {
        match self {
            RoomError::Auth(AuthError::Timeout) => true,
            RoomError::Auth(AuthError::Api(ApiError::Network(_))) => true,
            RoomError::Auth(AuthError::Api(ApiError::ServerError { status, .. })) => {
                *status >= 500 || *status == 429
            }
            _ => false,
        }
    }
}

/// An admitted participant in one meeting.
pub struct RoomSession {
    creds: Credentials,
    meeting_id: String,
    user_id: String,
    /// Room token from the admission response and when it arrived, used
    /// for the first connect if it is still young (see [`PENDING_MAX_AGE`]).
    pending: Option<(SecretString, Instant)>,
}

/// The admission token is used for the first connect attempt only if it
/// is younger than this. meeting-api expires CLI room tokens after 300 s;
/// anything older is dropped and a new one minted.
const PENDING_MAX_AGE: Duration = Duration::from_secs(60);

impl RoomSession {
    /// Join `meeting_id`, waiting for the host to start the meeting or admit
    /// us as needed. Returns once admitted.
    pub async fn join(
        creds: Credentials,
        meeting_id: &str,
        display_name: Option<String>,
    ) -> Result<Self, RoomError> {
        let id = meeting_id.to_string();
        // Check the meeting exists (the CLI never creates one) and hasn't
        // ended before joining.
        if meeting_phase(&get_meeting_state(&creds, &id).await?) == MeetingPhase::Ended {
            return Err(RoomError::MeetingEnded(id));
        }

        loop {
            let joined = creds
                .call(|client| {
                    let id = id.clone();
                    let name = display_name.clone();
                    async move { client.join_meeting(&id, name.as_deref()).await }
                })
                .await
                .map_err(|e| map_meeting_error(e, &id))?;
            match joined.status.as_str() {
                "waiting_for_meeting" => {
                    println!(
                        "Meeting '{id}' hasn't started yet. Waiting for the host to start it..."
                    );
                    wait_until_active(&creds, &id).await?;
                }
                "waiting" => {
                    println!("Waiting for the host to admit you to meeting '{id}'...");
                    let admitted = wait_for_admission(&creds, &id).await?;
                    return Self::admitted(creds, id, admitted);
                }
                _ => return Self::admitted(creds, id, joined),
            }
        }
    }

    fn admitted(
        creds: Credentials,
        meeting_id: String,
        status: ParticipantStatusResponse,
    ) -> Result<Self, RoomError> {
        let token = room_token_from(&meeting_id, &status)?;
        Ok(Self {
            creds,
            meeting_id,
            user_id: status.user_id,
            pending: Some((token, Instant::now())),
        })
    }

    /// The authenticated user id, stamped on every packet.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The meeting id.
    pub fn meeting_id(&self) -> &str {
        &self.meeting_id
    }

    /// A room token for one relay connect attempt. The first call returns
    /// the token from admission if it is younger than [`PENDING_MAX_AGE`];
    /// every other call mints a new one. A token is never used for more
    /// than one attempt.
    pub async fn next_room_token(&mut self) -> Result<SecretString, RoomError> {
        if let Some(token) = take_if_young(&mut self.pending, Instant::now()) {
            return Ok(token);
        }
        let status = current_status(&self.creds, &self.meeting_id).await?;
        room_token_from(&self.meeting_id, &status)
    }
}

/// Take the admission token out (so it is never reused) and return it
/// only if it is still younger than [`PENDING_MAX_AGE`] at `now`.
fn take_if_young(
    pending: &mut Option<(SecretString, Instant)>,
    now: Instant,
) -> Option<SecretString> {
    let (token, issued) = pending.take()?;
    (now.saturating_duration_since(issued) < PENDING_MAX_AGE).then_some(token)
}

fn room_token_from(
    meeting_id: &str,
    status: &ParticipantStatusResponse,
) -> Result<SecretString, RoomError> {
    match status.status.as_str() {
        "admitted" => status
            .room_token
            .clone()
            .map(SecretString::from)
            .ok_or(RoomError::MissingRoomToken),
        "rejected" => Err(RoomError::Rejected(meeting_id.to_string())),
        other => Err(RoomError::NotAdmitted(
            meeting_id.to_string(),
            other.to_string(),
        )),
    }
}

fn map_meeting_error(err: AuthError, meeting_id: &str) -> RoomError {
    match err {
        // 404 is either MEETING_NOT_FOUND or NOT_IN_MEETING (e.g. removed).
        AuthError::Api(ApiError::NotFound(body)) if body.contains("NOT_IN_MEETING") => {
            RoomError::NotAdmitted(meeting_id.to_string(), "not in meeting".to_string())
        }
        AuthError::Api(ApiError::NotFound(_)) => RoomError::MeetingNotFound(meeting_id.to_string()),
        AuthError::Api(ApiError::MeetingNotActive) => {
            RoomError::MeetingEnded(meeting_id.to_string())
        }
        other => RoomError::Auth(other),
    }
}

async fn get_meeting_state(creds: &Credentials, id: &str) -> Result<String, RoomError> {
    let owned = id.to_string();
    creds
        .call(|client| {
            let id = owned.clone();
            async move { client.get_meeting(&id).await }
        })
        .await
        .map(|meeting| meeting.state)
        .map_err(|e| map_meeting_error(e, id))
}

async fn current_status(
    creds: &Credentials,
    id: &str,
) -> Result<ParticipantStatusResponse, RoomError> {
    let owned = id.to_string();
    creds
        .call(|client| {
            let id = owned.clone();
            async move { client.get_status(&id).await }
        })
        .await
        .map_err(|e| map_meeting_error(e, id))
}

/// Where a meeting is in its lifecycle, from `MeetingInfoResponse.state`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MeetingPhase {
    /// `active`: the host is in; joining can proceed.
    Active,
    /// `ended`: over. The CLI exits rather than waiting.
    Ended,
    /// `idle` (or anything unknown): not started yet.
    NotStarted,
}

pub(crate) fn meeting_phase(state: &str) -> MeetingPhase {
    match state {
        "active" => MeetingPhase::Active,
        "ended" => MeetingPhase::Ended,
        _ => MeetingPhase::NotStarted,
    }
}

/// Poll cadence: about every 3 s while things are healthy; after a
/// retryable failure, back off from about 6 s (never sooner than the
/// healthy cadence) up to 30 s until a request succeeds again.
pub(crate) struct Poller {
    backoff: Backoff,
    failing: bool,
}

impl Poller {
    pub(crate) fn new() -> Self {
        Self {
            // Equal jitter halves the nominal delay: the first retry lands
            // in 6..12 s, above the 2.5..3.5 s healthy cadence.
            backoff: Backoff::new(Duration::from_secs(12), Duration::from_secs(30)),
            failing: false,
        }
    }

    pub(crate) fn next_delay(&mut self) -> Duration {
        if self.failing {
            self.backoff.next_delay()
        } else {
            poll_delay()
        }
    }

    pub(crate) fn succeeded(&mut self) {
        self.failing = false;
        self.backoff.reset();
    }

    pub(crate) fn failed(&mut self) {
        self.failing = true;
    }
}

/// Poll the meeting until the host starts it; fail if it ends instead.
async fn wait_until_active(creds: &Credentials, id: &str) -> Result<(), RoomError> {
    let mut poller = Poller::new();
    loop {
        tokio::time::sleep(poller.next_delay()).await;
        match get_meeting_state(creds, id).await {
            Ok(state) => {
                poller.succeeded();
                match meeting_phase(&state) {
                    MeetingPhase::Active => return Ok(()),
                    MeetingPhase::Ended => return Err(RoomError::MeetingEnded(id.to_string())),
                    MeetingPhase::NotStarted => {}
                }
            }
            Err(e) if e.is_retryable() => {
                poller.failed();
                tracing::warn!("checking meeting state failed: {e}");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Poll our participant status until admitted, rejected, or the meeting ends.
async fn wait_for_admission(
    creds: &Credentials,
    id: &str,
) -> Result<ParticipantStatusResponse, RoomError> {
    let mut poller = Poller::new();
    loop {
        tokio::time::sleep(poller.next_delay()).await;
        match current_status(creds, id).await {
            Ok(status) => {
                poller.succeeded();
                match status.status.as_str() {
                    "waiting" => {}
                    "admitted" => return Ok(status),
                    "rejected" => return Err(RoomError::Rejected(id.to_string())),
                    other => return Err(RoomError::NotAdmitted(id.to_string(), other.to_string())),
                }
            }
            Err(e) if e.is_retryable() => {
                poller.failed();
                tracing::warn!("checking admission failed: {e}");
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn status(s: &str, token: Option<&str>) -> ParticipantStatusResponse {
        ParticipantStatusResponse {
            user_id: "u".into(),
            display_name: None,
            status: s.into(),
            is_host: false,
            joined_at: 0,
            admitted_at: None,
            room_token: token.map(String::from),
            observer_token: None,
            waiting_room_enabled: None,
            host_display_name: None,
            host_user_id: None,
        }
    }

    #[test]
    fn token_from_status() {
        let t = room_token_from("m", &status("admitted", Some("jwt"))).unwrap();
        assert_eq!(t.expose_secret(), "jwt");
        assert!(matches!(
            room_token_from("m", &status("admitted", None)),
            Err(RoomError::MissingRoomToken)
        ));
        assert!(matches!(
            room_token_from("m", &status("rejected", None)),
            Err(RoomError::Rejected(_))
        ));
        assert!(matches!(
            room_token_from("m", &status("left", None)),
            Err(RoomError::NotAdmitted(_, ref s)) if s == "left"
        ));
    }

    #[test]
    fn admission_token_is_used_once_and_only_while_young() {
        let issued = Instant::now();
        let token = || Some((SecretString::from("jwt".to_string()), issued));

        let mut pending = token();
        let first = take_if_young(&mut pending, issued + Duration::from_secs(1));
        assert_eq!(first.unwrap().expose_secret(), "jwt");
        assert!(pending.is_none(), "taken, never reused");
        assert!(take_if_young(&mut pending, issued).is_none());

        let mut pending = token();
        assert!(take_if_young(&mut pending, issued + PENDING_MAX_AGE).is_none());
        assert!(pending.is_none(), "a stale token is dropped, not kept");
    }

    #[test]
    fn retryable_errors() {
        assert!(RoomError::Auth(AuthError::Timeout).is_retryable());
        assert!(RoomError::Auth(AuthError::Api(ApiError::ServerError {
            status: 502,
            body: String::new()
        }))
        .is_retryable());
        assert!(!RoomError::Auth(AuthError::Api(ApiError::NotAuthenticated)).is_retryable());
        assert!(
            !RoomError::Auth(AuthError::Api(ApiError::Forbidden(String::new()))).is_retryable()
        );
        assert!(!RoomError::MeetingNotFound("m".into()).is_retryable());
        assert!(!RoomError::MeetingEnded("m".into()).is_retryable());
        assert!(!RoomError::Auth(AuthError::NotLoggedIn("h".into())).is_retryable());
    }

    #[test]
    fn meeting_error_mapping() {
        let not_found = r#"{"success":false,"result":{"code":"MEETING_NOT_FOUND"}}"#;
        assert!(matches!(
            map_meeting_error(AuthError::Api(ApiError::NotFound(not_found.into())), "m"),
            RoomError::MeetingNotFound(_)
        ));
        let not_in = r#"{"success":false,"result":{"code":"NOT_IN_MEETING"}}"#;
        assert!(matches!(
            map_meeting_error(AuthError::Api(ApiError::NotFound(not_in.into())), "m"),
            RoomError::NotAdmitted(..)
        ));
        assert!(matches!(
            map_meeting_error(AuthError::Api(ApiError::MeetingNotActive), "m"),
            RoomError::MeetingEnded(_)
        ));
    }

    #[test]
    fn meeting_phases() {
        assert_eq!(meeting_phase("active"), MeetingPhase::Active);
        assert_eq!(meeting_phase("ended"), MeetingPhase::Ended);
        assert_eq!(meeting_phase("idle"), MeetingPhase::NotStarted);
        assert_eq!(meeting_phase("something-new"), MeetingPhase::NotStarted);
    }

    #[test]
    fn poller_backs_off_on_errors_and_resets() {
        let mut poller = Poller::new();
        let healthy = poller.next_delay();
        assert!(healthy >= Duration::from_millis(2500) && healthy < Duration::from_millis(3500));

        poller.failed();
        let mut nominal_max = Duration::ZERO;
        for _ in 0..10 {
            let d = poller.next_delay();
            // Never sooner than the healthy cadence's upper bound.
            assert!(d >= Duration::from_secs(6) && d <= Duration::from_secs(30));
            nominal_max = nominal_max.max(d);
        }
        assert!(nominal_max >= Duration::from_secs(15), "{nominal_max:?}");

        poller.succeeded();
        let d = poller.next_delay();
        assert!(d >= Duration::from_millis(2500) && d < Duration::from_millis(3500));
        poller.failed();
        let first_retry = poller.next_delay();
        assert!(
            first_retry >= Duration::from_secs(6) && first_retry <= Duration::from_secs(12),
            "{first_retry:?}"
        );
    }
}
