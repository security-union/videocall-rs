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

//! Joining a meeting through meeting-api with this machine's device key,
//! and minting the room token the relay's `/lobby` requires.

use std::future::Future;
use std::time::Duration;

use anyhow::bail;
use secrecy::SecretString;
use videocall_meeting_client::ApiError;

use crate::auth::{AuthError, Credentials};
use crate::backoff::{poll_delay, Backoff};

/// Network trouble or a 5xx/429 from meeting-api: worth trying again.
pub fn is_transient(error: &AuthError) -> bool {
    match error {
        AuthError::Api(ApiError::Network(_)) => true,
        AuthError::Api(ApiError::ServerError { status, .. }) => *status >= 500 || *status == 429,
        _ => false,
    }
}

/// A plain-English error for a meeting-api failure about `meeting_id`.
pub fn meeting_error(error: AuthError, meeting_id: &str) -> anyhow::Error {
    match error {
        AuthError::Api(ApiError::NotFound(_)) => anyhow::anyhow!(
            "meeting '{meeting_id}' doesn't exist or you are not in it. Create it in a browser \
             first; the CLI can only join existing meetings."
        ),
        AuthError::Api(ApiError::MeetingNotActive) => {
            anyhow::anyhow!("meeting '{meeting_id}' has ended")
        }
        other => other.into(),
    }
}

/// Backoff for meeting-api while it is failing: the first retry waits
/// 6 to 12 s (above the ~3 s healthy poll), growing to 15 to 30 s.
fn api_backoff() -> Backoff {
    Backoff::new(Duration::from_secs(12), Duration::from_secs(30))
}

/// Run `call` until it succeeds or fails for good. Transient errors are
/// logged and retried with [`api_backoff`], so a Pi that boots before its
/// network is up, or a meeting-api restart, doesn't end the run.
async fn retry<T, Fut>(call: impl FnMut() -> Fut) -> Result<T, AuthError>
where
    Fut: Future<Output = Result<T, AuthError>>,
{
    retry_with(api_backoff(), call).await
}

async fn retry_with<T, Fut>(
    mut backoff: Backoff,
    mut call: impl FnMut() -> Fut,
) -> Result<T, AuthError>
where
    Fut: Future<Output = Result<T, AuthError>>,
{
    loop {
        match call().await {
            Err(e) if is_transient(&e) => {
                let delay = backoff.next_delay();
                tracing::warn!("meeting-api: {e}; retrying in {delay:.1?}");
                tokio::time::sleep(delay).await;
            }
            other => return other,
        }
    }
}

/// Every ~3 s (2.5 to 3.5 s, jittered so many CLIs don't poll in step),
/// run `call` until `done` returns a result.
async fn poll<T, Fut>(
    meeting_id: &str,
    mut call: impl FnMut() -> Fut,
    mut done: impl FnMut(T) -> Option<anyhow::Result<()>>,
) -> anyhow::Result<()>
where
    Fut: Future<Output = Result<T, AuthError>>,
{
    loop {
        tokio::time::sleep(poll_delay()).await;
        let value = retry(&mut call)
            .await
            .map_err(|e| meeting_error(e, meeting_id))?;
        if let Some(result) = done(value) {
            return result;
        }
    }
}

/// Join `meeting_id` and wait until the meeting is running and the host has
/// admitted us. Returns the user id meeting-api assigned.
pub async fn join(
    creds: &Credentials,
    meeting_id: &str,
    display_name: Option<&str>,
) -> anyhow::Result<String> {
    let id = meeting_id.to_string();
    let name = display_name.map(str::to_string);
    loop {
        let joined = retry(|| {
            let (id, name) = (id.clone(), name.clone());
            creds.call(move |client| {
                let (id, name) = (id.clone(), name.clone());
                async move { client.join_meeting(&id, name.as_deref()).await }
            })
        })
        .await
        .map_err(|e| meeting_error(e, &id))?;
        match joined.status.as_str() {
            "admitted" => return Ok(joined.user_id),
            "rejected" => bail!("the host declined your request to join meeting '{id}'"),
            "waiting_for_meeting" => {
                println!("Meeting '{id}' hasn't started yet. Waiting for the host...");
                poll(
                    &id,
                    || {
                        let id = id.clone();
                        creds.call(move |client| {
                            let id = id.clone();
                            async move { client.get_meeting(&id).await }
                        })
                    },
                    |meeting| match meeting.state.as_str() {
                        "active" => Some(Ok(())),
                        "ended" => Some(Err(anyhow::anyhow!("meeting '{id}' has ended"))),
                        _ => None,
                    },
                )
                .await?;
                // Started: join again to enter it (or its waiting room).
            }
            "waiting" => {
                println!("Waiting for the host to admit you to meeting '{id}'...");
                poll(
                    &id,
                    || {
                        let id = id.clone();
                        creds.call(move |client| {
                            let id = id.clone();
                            async move { client.get_status(&id).await }
                        })
                    },
                    |status| match status.status.as_str() {
                        "waiting" => None,
                        "admitted" => Some(Ok(())),
                        other => Some(Err(not_admitted(&id, other))),
                    },
                )
                .await?;
                return Ok(joined.user_id);
            }
            other => bail!("unexpected join status for meeting '{id}': {other}"),
        }
    }
}

fn not_admitted(meeting_id: &str, status: &str) -> anyhow::Error {
    match status {
        "rejected" => {
            anyhow::anyhow!("the host declined your request to join meeting '{meeting_id}'")
        }
        other => anyhow::anyhow!(
            "you are no longer admitted to meeting '{meeting_id}' (status: {other}); the host \
             may have removed you. Run the command again to ask to join."
        ),
    }
}

/// Why no room token could be minted.
#[derive(Debug, thiserror::Error)]
pub enum RoomTokenError {
    #[error(transparent)]
    Api(#[from] AuthError),
    /// meeting-api answered, but we are not admitted any more.
    #[error("{0}")]
    NotAdmitted(String),
}

impl RoomTokenError {
    /// Worth another attempt: only meeting-api or network trouble.
    pub fn is_transient(&self) -> bool {
        matches!(self, RoomTokenError::Api(e) if is_transient(e))
    }

    /// The error to exit with.
    pub fn into_exit(self, meeting_id: &str) -> anyhow::Error {
        match self {
            RoomTokenError::Api(e) => meeting_error(e, meeting_id),
            RoomTokenError::NotAdmitted(message) => anyhow::anyhow!(message),
        }
    }
}

/// A room token for one relay connection attempt. Never reuse one: CLI room
/// tokens expire after 300 s. Not being admitted any more (removed by the
/// host, sent back to the waiting room) is final: the run ends and a
/// restart asks to join again.
pub async fn room_token(
    creds: &Credentials,
    meeting_id: &str,
) -> Result<SecretString, RoomTokenError> {
    let id = meeting_id.to_string();
    let status = creds
        .call(|client| {
            let id = id.clone();
            async move { client.get_status(&id).await }
        })
        .await?;
    match (status.status.as_str(), status.room_token) {
        ("admitted", Some(token)) => Ok(SecretString::from(token)),
        (other, _) => Err(RoomTokenError::NotAdmitted(
            not_admitted(meeting_id, other).to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors() {
        let server = |status| {
            AuthError::Api(ApiError::ServerError {
                status,
                body: String::new(),
            })
        };
        assert!(is_transient(&server(502)));
        assert!(is_transient(&server(429)));
        assert!(!is_transient(&server(400)));
        assert!(!is_transient(&AuthError::DeviceRevoked));
        assert!(!is_transient(&AuthError::Api(ApiError::NotFound(
            String::new()
        ))));
        assert!(RoomTokenError::Api(server(503)).is_transient());
        assert!(!RoomTokenError::NotAdmitted("x".into()).is_transient());
    }

    #[test]
    fn not_admitted_messages() {
        assert!(not_admitted("m", "rejected")
            .to_string()
            .contains("declined"));
        let removed = not_admitted("m", "waiting").to_string();
        assert!(removed.contains("no longer admitted") && removed.contains("waiting"));
    }

    #[tokio::test]
    async fn retry_retries_transient_errors_only() {
        let fast = || Backoff::new(Duration::from_millis(1), Duration::from_millis(1));
        let mut calls = 0;
        let result: Result<u8, AuthError> = retry_with(fast(), || {
            calls += 1;
            let n = calls;
            async move {
                match n {
                    1 => Err(AuthError::Api(ApiError::ServerError {
                        status: 503,
                        body: String::new(),
                    })),
                    _ => Ok(7),
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, 2);

        let mut calls = 0;
        let result: Result<u8, AuthError> = retry_with(fast(), || {
            calls += 1;
            async { Err(AuthError::DeviceRevoked) }
        })
        .await;
        assert!(matches!(result, Err(AuthError::DeviceRevoked)));
        assert_eq!(calls, 1);
    }
}
