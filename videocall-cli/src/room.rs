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

use std::time::Duration;

use anyhow::bail;
use secrecy::SecretString;
use videocall_meeting_client::ApiError;

use crate::auth::{AuthError, Credentials};

const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Network trouble or a 5xx/429 from meeting-api: worth trying again.
pub fn is_transient(error: &AuthError) -> bool {
    match error {
        AuthError::Api(ApiError::Network(_)) => true,
        AuthError::Api(ApiError::ServerError { status, .. }) => *status >= 500 || *status == 429,
        _ => false,
    }
}

fn meeting_error(error: AuthError, meeting_id: &str) -> anyhow::Error {
    match error {
        AuthError::Api(ApiError::NotFound(_)) => anyhow::anyhow!(
            "meeting '{meeting_id}' doesn't exist or you are not in it. Create it with \
             `videocall-cli meeting create --meeting-id {meeting_id}`, or in a browser."
        ),
        AuthError::Api(ApiError::MeetingNotActive) => {
            anyhow::anyhow!("meeting '{meeting_id}' has ended")
        }
        other => other.into(),
    }
}

async fn poll<T, Fut>(
    mut call: impl FnMut() -> Fut,
    mut done: impl FnMut(T) -> Option<anyhow::Result<()>>,
) -> anyhow::Result<()>
where
    Fut: std::future::Future<Output = anyhow::Result<T, AuthError>>,
{
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match call().await {
            Ok(value) => {
                if let Some(result) = done(value) {
                    return result;
                }
            }
            Err(e) if is_transient(&e) => tracing::warn!("meeting-api: {e}; retrying"),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Returns the user id meeting-api assigned.
pub async fn join(creds: &Credentials, meeting_id: &str) -> anyhow::Result<String> {
    let id = meeting_id.to_string();
    loop {
        let joined = creds
            .call(|client| {
                let id = id.clone();
                async move { client.join_meeting(&id, None).await }
            })
            .await
            .map_err(|e| meeting_error(e, &id))?;
        match joined.status.as_str() {
            "admitted" => return Ok(joined.user_id),
            "rejected" => bail!("the host declined your request to join meeting '{id}'"),
            "waiting_for_meeting" => {
                println!("Meeting '{id}' hasn't started yet. Waiting for the host...");
                poll(
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
                        "rejected" => Some(Err(anyhow::anyhow!(
                            "the host declined your request to join meeting '{id}'"
                        ))),
                        other => Some(Err(anyhow::anyhow!(
                            "you are not admitted to meeting '{id}' (status: {other})"
                        ))),
                    },
                )
                .await?;
                return Ok(joined.user_id);
            }
            other => bail!("unexpected join status for meeting '{id}': {other}"),
        }
    }
}

/// Never reuse one: CLI room tokens expire after 300 s.
pub async fn room_token(creds: &Credentials, meeting_id: &str) -> Result<SecretString, AuthError> {
    let id = meeting_id.to_string();
    creds
        .call(|client| {
            let id = id.clone();
            async move { client.refresh_room_token(&id).await }
        })
        .await
        .map(SecretString::from)
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
    }
}
