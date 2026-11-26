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

use anyhow::{bail, Error};
use protobuf::Message;
use secrecy::ExposeSecret;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::{
        mpsc::{self, Sender},
        watch,
    },
    time::{self, Duration},
};
use tracing::info;
use url::Url;
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    media_packet::{media_packet::MediaType, HeartbeatMetadata, MediaPacket},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

use crate::auth::Credentials;
use crate::backoff::Backoff;
use crate::cli_args::Stream;
use crate::net_policy::is_local_host;
use crate::room;

use super::camera_synk::CameraSynk;

/// Consecutive failed sends after which the relay connection counts as
/// lost even if QUIC hasn't reported it closed yet (a few seconds of media).
const MAX_CONSECUTIVE_SEND_FAILURES: u32 = 100;

/// How long a clean shutdown waits for the close to reach the relay.
pub const CLOSE_GRACE: Duration = Duration::from_millis(500);

/// Records why the relay connection ended. The first reason wins.
#[derive(Clone)]
struct LossSignal {
    reason: Arc<watch::Sender<Option<String>>>,
    consecutive_failures: Arc<AtomicU32>,
}

impl LossSignal {
    fn new() -> Self {
        Self {
            reason: Arc::new(watch::Sender::new(None)),
            consecutive_failures: Arc::new(AtomicU32::new(0)),
        }
    }

    fn mark_lost(&self, reason: String) {
        self.reason.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }

    fn reason(&self) -> Option<String> {
        self.reason.borrow().clone()
    }

    /// Count a send result. Failures on a closed session, or too many in a
    /// row, mark the connection lost. Returns false if it is lost.
    fn record_send(
        &self,
        session: &web_transport_quinn::Session,
        result: anyhow::Result<()>,
    ) -> bool {
        match result {
            Ok(()) => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(closed) = session.close_reason() {
                    self.mark_lost(closed.to_string());
                } else if failures >= MAX_CONSECUTIVE_SEND_FAILURES {
                    self.mark_lost(format!("{failures} sends in a row failed, last: {e}"));
                } else {
                    tracing::warn!("Failed to send to the relay: {e}");
                }
            }
        }
        self.reason().is_none()
    }

    async fn wait(&self) -> String {
        let mut rx = self.reason.subscribe();
        let reason = match rx.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone(),
            // The sender lives in `self`, so this can't happen.
            Err(_) => None,
        };
        match reason {
            Some(reason) => reason,
            None => std::future::pending().await,
        }
    }
}

pub struct WebTransportClient {
    options: Stream,
    creds: Credentials,
    /// The account this device acts as, from meeting-api.
    user_id: String,
    sender: Option<Sender<Vec<u8>>>,
    session: Option<web_transport_quinn::Session>,
    loss: LossSignal,
}

impl WebTransportClient {
    /// A client for an admitted participant; packets carry `user_id`.
    pub fn new(options: Stream, creds: Credentials, user_id: String) -> Self {
        Self {
            options,
            creds,
            user_id,
            sender: None,
            session: None,
            loss: LossSignal::new(),
        }
    }

    async fn send_connection_packet(&self) -> anyhow::Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.options.meeting_id.clone(),
            ..Default::default()
        };
        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            user_id: self.user_id.as_bytes().to_vec(),
            data: connection_packet.write_to_bytes()?,
            ..Default::default()
        };
        self.queue_message(packet.write_to_bytes()?).await?;
        Ok(())
    }

    pub async fn send(session: &web_transport_quinn::Session, data: Vec<u8>) -> anyhow::Result<()> {
        let mut stream = session.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish()?;
        Ok(())
    }

    async fn queue_message(&self, message: Vec<u8>) -> anyhow::Result<()> {
        if let Some(reason) = self.loss.reason() {
            bail!("relay connection lost: {reason}");
        }
        if let Some(sender) = &self.sender {
            sender
                .send(message)
                .await
                .map_err(|_| Error::msg("Failed to send message to queue"))
        } else {
            Err(Error::msg("No sender available"))
        }
    }

    async fn start_heartbeat(&self, session: web_transport_quinn::Session, options: &Stream) {
        let interval = time::interval(Duration::from_secs(1));
        let email = self.user_id.clone();
        // Advertise audio as enabled only when we are actually capturing a mic
        // (`--audio-device` was provided). Otherwise peers would see the CLI as
        // unmuted while it sends no audio — and vice-versa, a streamed mic would
        // be shown muted. Video is always on in stream mode.
        let audio_enabled = options.audio_device.is_some();
        let loss = self.loss.clone();
        tokio::spawn(async move {
            let mut interval = interval;
            while loss.reason().is_none() {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis(); // Get milliseconds since Unix epoch
                interval.tick().await;
                let actual_heartbeat = MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    user_id: email.as_bytes().to_vec(),
                    timestamp: now_ms as f64,
                    heartbeat_metadata: Some(HeartbeatMetadata {
                        video_enabled: true,
                        audio_enabled,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };

                let packet = PacketWrapper {
                    user_id: email.as_bytes().to_vec(),
                    packet_type: PacketType::MEDIA.into(),
                    data: actual_heartbeat.write_to_bytes().unwrap(),
                    ..Default::default()
                };
                let data = packet.write_to_bytes().unwrap();
                loss.record_send(&session, Self::send(&session, data).await);
            }
        });
    }
}

/// Refuse `--insecure-skip-verify` unless the relay is local.
///
/// Skipping certificate checks against a public host would hand the room
/// token to anyone on the path, so it is only allowed for `localhost`,
/// loopback, and private-range addresses (a dev stack with a self-signed
/// certificate).
pub fn check_insecure_skip_verify(relay: &Url, insecure_skip_verify: bool) -> anyhow::Result<()> {
    if insecure_skip_verify && !is_local_host(relay) {
        bail!(
            "--insecure-skip-verify is only allowed for localhost, loopback, or private-range \
             relay addresses, not {}. Remove the flag to verify the relay's certificate.",
            relay.host_str().unwrap_or("<no host>")
        );
    }
    Ok(())
}

/// `{relay}/lobby?token=<room token>`. Any path or query on `relay` is
/// replaced. The result contains the token: never log it.
pub fn lobby_url(relay: &Url, room_token: &str) -> Url {
    let mut url = relay.clone();
    url.set_path("/lobby");
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut().append_pair("token", room_token);
    url
}

/// The HTTP status of a refused WebTransport CONNECT, if that is the error.
///
/// web-transport-quinn 0.8.1 keeps its connect error types private, so the
/// status is read from the `Debug` text. A non-2xx response surfaces as
/// `HttpError(ProtoError(WrongStatus(Some(401))))`; `ErrorStatus(401)` is the
/// crate's other path for the same thing. Same approach as the relay tests in
/// `actix-api/src/webtransport/mod.rs`.
pub fn rejection_status(err: &web_transport_quinn::ClientError) -> Option<u16> {
    let web_transport_quinn::ClientError::HttpError(inner) = err else {
        return None;
    };
    parse_rejection_status(&format!("{inner:?}"))
}

fn parse_rejection_status(debug_text: &str) -> Option<u16> {
    ["WrongStatus(Some(", "ErrorStatus("]
        .iter()
        .find_map(|marker| debug_text.split(marker).nth(1))
        .and_then(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
}

/// Connect to the relay, minting a fresh room token for every initial
/// connect attempt. Once a session is established this returns; a later
/// drop ends the stream (see [`CameraSynk::lost`]) instead of reconnecting.
///
/// Transient failures back off exponentially (1 s to 30 s, jittered).
/// A 401/403/404 from the relay, or a non-retryable meeting-api error (not
/// logged in, meeting gone, no longer admitted), ends the loop with an error.
async fn connect_to_server(
    options: &Stream,
    creds: &Credentials,
) -> anyhow::Result<web_transport_quinn::Session> {
    check_insecure_skip_verify(&options.url, options.insecure_skip_verify)?;
    let client = if options.insecure_skip_verify {
        tracing::warn!("Skipping TLS certificate verification for the local relay");
        // SAFETY: gated above to localhost / loopback / private addresses.
        unsafe { web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()? }
    } else {
        web_transport_quinn::ClientBuilder::new().with_system_roots()?
    };

    let mut backoff = Backoff::relay();
    let mut previous_was_401 = false;
    loop {
        // A fresh room token for every attempt: they expire after 300 s.
        let token = match room::room_token(creds, &options.meeting_id).await {
            Ok(token) => token,
            Err(e) if e.is_transient() => {
                let delay = backoff.next_delay();
                tracing::warn!("Couldn't get a room token: {e}. Retrying in {delay:.1?}...");
                time::sleep(delay).await;
                continue;
            }
            Err(e) => return Err(e.into_exit(&options.meeting_id)),
        };

        info!("Connecting to relay {}", options.url);
        let url = lobby_url(&options.url, token.expose_secret());
        let (status, error) = match time::timeout(CONNECT_TIMEOUT, client.connect(url)).await {
            Ok(Ok(session)) => {
                info!("WebTransport session established");
                return Ok(session);
            }
            Ok(Err(e)) => (rejection_status(&e), e.to_string()),
            Err(_) => (
                None,
                format!("no answer within {} s", CONNECT_TIMEOUT.as_secs()),
            ),
        };
        match after_connect_failure(status, previous_was_401) {
            NextStep::RetryNow => {
                previous_was_401 = true;
                info!("Relay said the room token expired (401); retrying with a fresh one");
            }
            NextStep::Backoff => {
                previous_was_401 = false;
                let delay = backoff.next_delay();
                tracing::error!(
                    "WebTransport connection failed: {error}. Retrying in {delay:.1?}..."
                );
                time::sleep(delay).await;
            }
            NextStep::Fatal(reason) => bail!("{reason} (relay {})", options.url),
        }
    }
}

/// How long one WebTransport CONNECT may take. Shorter than QUIC's idle
/// timeout so a network that drops UDP gets the backoff, not a long hang.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// What to do after a failed relay connect attempt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NextStep {
    /// Mint a fresh room token and try again right away.
    RetryNow,
    /// Sleep for the next backoff delay, then try again.
    Backoff,
    /// Stop with this message.
    Fatal(String),
}

/// 401 means "come back with a fresh token": retry once immediately, and
/// give up only if the very next attempt gets 401 too. 403 and 404 are
/// terminal. Anything else (network, QUIC, timeout, 5xx) backs off.
pub(crate) fn after_connect_failure(status: Option<u16>, previous_was_401: bool) -> NextStep {
    match status {
        Some(401) if !previous_was_401 => NextStep::RetryNow,
        Some(401) => NextStep::Fatal(
            "the relay rejected a freshly minted room token twice (401)".to_string(),
        ),
        Some(403) => NextStep::Fatal("the relay rejected the room token (403)".to_string()),
        Some(404) => {
            NextStep::Fatal("the relay has no /lobby endpoint (404); check --url".to_string())
        }
        _ => NextStep::Backoff,
    }
}

impl CameraSynk for WebTransportClient {
    async fn connect(&mut self) -> anyhow::Result<()> {
        let session = connect_to_server(&self.options, &self.creds).await?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        self.sender = Some(tx);
        self.session = Some(session.clone());

        // The relay (or the network) closed the session.
        let (watched, loss) = (session.clone(), self.loss.clone());
        tokio::spawn(async move {
            let reason = watched.closed().await;
            loss.mark_lost(reason.to_string());
        });

        // Spawn a task to handle sending messages via the WebTransport session
        let (session_clone, loss) = (session.clone(), self.loss.clone());
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let session_clone_inner = session_clone.clone();
                let loss = loss.clone();
                tokio::spawn(async move {
                    let result = WebTransportClient::send(&session_clone_inner, message).await;
                    loss.record_send(&session_clone_inner, result);
                });
            }
        });

        // Spawn a separate task for heartbeat
        self.start_heartbeat(session.clone(), &self.options).await;

        self.send_connection_packet().await?;
        Ok(())
    }

    async fn send_packet(&self, data: Vec<u8>) -> anyhow::Result<()> {
        self.queue_message(data).await
    }

    async fn lost(&self) -> String {
        if self.session.is_none() {
            return std::future::pending().await;
        }
        self.loss.wait().await
    }

    /// Close with code 0 and give the close frame up to [`CLOSE_GRACE`] to
    /// leave, so the relay drops us now instead of at the QUIC idle timeout.
    async fn close(&self) {
        let Some(session) = &self.session else {
            return;
        };
        self.loss.mark_lost("closed by this client".to_string());
        session.close(0, b"client shutting down");
        // `closed()` resolves once the local close is applied; the short
        // sleep after it lets quinn's connection driver put CONNECTION_CLOSE
        // on the wire before the runtime shuts down. Bounded either way.
        let _ = time::timeout(CLOSE_GRACE, async {
            session.closed().await;
            time::sleep(Duration::from_millis(100)).await;
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loss_signal_keeps_the_first_reason_and_wakes_waiters() {
        let loss = LossSignal::new();
        assert!(loss.reason().is_none());
        let waiter = tokio::spawn({
            let loss = loss.clone();
            async move { loss.wait().await }
        });
        loss.mark_lost("relay closed the session".into());
        loss.mark_lost("later reason".into());
        assert_eq!(waiter.await.unwrap(), "relay closed the session");
        assert_eq!(loss.reason().as_deref(), Some("relay closed the session"));
    }

    #[test]
    fn lobby_url_construction() {
        let relay = Url::parse("https://relay.example:4433").unwrap();
        let url = lobby_url(&relay, "a.b+c/d=");
        assert_eq!(url.path(), "/lobby");
        assert_eq!(url.host_str(), Some("relay.example"));
        assert_eq!(url.port(), Some(4433));
        let pairs: Vec<_> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "token");
        assert_eq!(pairs[0].1, "a.b+c/d=");

        // Stray path / query on --url is replaced, not appended to.
        let relay = Url::parse("https://relay.example/lobby/old/path?token=stale#x").unwrap();
        let url = lobby_url(&relay, "fresh");
        assert_eq!(url.as_str(), "https://relay.example/lobby?token=fresh");
    }

    #[test]
    fn insecure_skip_verify_gating() {
        let check = |url: &str, flag: bool| {
            check_insecure_skip_verify(&Url::parse(url).unwrap(), flag).is_ok()
        };
        assert!(check(
            "https://webtransport-us-east.webtransport.video",
            false
        ));
        assert!(!check(
            "https://webtransport-us-east.webtransport.video",
            true
        ));
        assert!(!check("https://8.8.8.8:4433", true));
        assert!(check("https://localhost:4433", true));
        assert!(check("https://127.0.0.1:4433", true));
        assert!(check("https://192.168.1.50:4433", true));
        assert!(check("https://[::1]:4433", true));
    }

    #[test]
    fn rejection_status_parsing() {
        assert_eq!(
            parse_rejection_status("ProtoError(WrongStatus(Some(401)))"),
            Some(401)
        );
        assert_eq!(parse_rejection_status("ErrorStatus(403)"), Some(403));
        assert_eq!(
            parse_rejection_status("ProtoError(WrongStatus(None))"),
            None
        );
        assert_eq!(parse_rejection_status("UnexpectedEnd"), None);
    }

    #[test]
    fn connect_failure_policy() {
        assert_eq!(after_connect_failure(Some(401), false), NextStep::RetryNow);
        assert!(matches!(
            after_connect_failure(Some(401), true),
            NextStep::Fatal(_)
        ));
        assert!(matches!(
            after_connect_failure(Some(403), false),
            NextStep::Fatal(_)
        ));
        assert!(matches!(
            after_connect_failure(Some(404), false),
            NextStep::Fatal(_)
        ));
        assert_eq!(after_connect_failure(Some(503), false), NextStep::Backoff);
        assert_eq!(after_connect_failure(None, false), NextStep::Backoff);
        // A timeout or network error after a 401 backs off and resets the
        // 401 streak (the caller clears `previous_was_401`).
        assert_eq!(after_connect_failure(None, true), NextStep::Backoff);
        assert!(CONNECT_TIMEOUT < Duration::from_secs(30));
    }
}
