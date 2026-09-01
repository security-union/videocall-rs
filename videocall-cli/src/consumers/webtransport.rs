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

use anyhow::Error;
use protobuf::Message;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::mpsc::{self, Sender},
    time::{self, Duration},
};
use tracing::info;
use url::Url;
use videocall_meeting_types::mint::{self, LobbyAuth};
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    media_packet::{media_packet::MediaType, HeartbeatMetadata, MediaPacket},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

use crate::cli_args::Stream;

use super::camera_synk::CameraSynk;

pub struct WebTransportClient {
    options: Stream,
    sender: Option<Sender<Vec<u8>>>,
}

impl WebTransportClient {
    pub fn new(options: Stream) -> Self {
        Self {
            options,
            sender: None,
        }
    }

    async fn send_connection_packet(&self) -> anyhow::Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.options.meeting_id.clone(),
            ..Default::default()
        };
        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            user_id: self.options.user_id.as_bytes().to_vec(),
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
        let email = options.user_id.clone();
        tokio::spawn(async move {
            let mut interval = interval;
            loop {
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
                if let Err(e) = Self::send(&session, data).await {
                    tracing::error!("Failed to send heartbeat: {}", e);
                }
            }
        });
    }
}

/// Build the lobby URL for one connection attempt.
pub fn lobby_url_for_attempt(options: &Stream, auth: &LobbyAuth) -> anyhow::Result<Url> {
    let mut base = options.url.clone();
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);

    let url = mint::build_lobby_url(base.as_str(), auth, &options.user_id, &options.meeting_id)?;
    Ok(Url::parse(&url)?)
}

async fn connect_to_server(options: &Stream) -> anyhow::Result<web_transport_quinn::Session> {
    // Resolved once so a missing credential fails immediately instead of
    // retrying forever (#2298).
    let auth = options.resolve_auth()?;

    loop {
        info!("Attempting to connect to {}", options.url);

        let url = lobby_url_for_attempt(options, &auth)?;

        // Create WebTransport client using 0.7.3 API (same pattern as bot)
        let client = if options.insecure_skip_verify {
            info!("WARNING: Skipping TLS certificate verification - connection is insecure!");
            web_transport_quinn::ClientBuilder::new()
                .dangerous()
                .with_no_certificate_verification()?
        } else {
            web_transport_quinn::ClientBuilder::new().with_system_roots()?
        };

        match client.connect(url).await {
            Ok(session) => {
                info!("WebTransport session established successfully");
                return Ok(session);
            }
            Err(e) => {
                tracing::error!(
                    "WebTransport connection failed: {}. Retrying in 5 seconds...",
                    e
                );
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

impl CameraSynk for WebTransportClient {
    async fn connect(&mut self) -> anyhow::Result<()> {
        let session = connect_to_server(&self.options).await?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        self.sender = Some(tx);

        // Spawn a task to handle sending messages via the WebTransport session
        let session_clone = session.clone();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let session_clone_inner = session_clone.clone();
                tokio::spawn(async move {
                    if let Err(e) = WebTransportClient::send(&session_clone_inner, message).await {
                        tracing::error!("Failed to send message: {}", e);
                    }
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
}

#[cfg(test)]
mod tests {
    use super::lobby_url_for_attempt;
    use crate::cli_args::{Mode, Opt, Stream};
    use clap::Parser;
    use videocall_meeting_types::mint::LobbyAuth;

    /// `--jwt-secret ""` neutralises the `JWT_SECRET` env fallback so these
    /// assertions hold on a developer machine that has the relay secret set.
    fn stream_from(extra: &[&str]) -> Stream {
        let mut argv = vec![
            "videocall-cli",
            "stream",
            "--url",
            "https://relay.example.com",
            "--user-id",
            "cli-1",
            "--meeting-id",
            "room-1",
        ];
        argv.extend_from_slice(extra);
        match Opt::parse_from(argv).mode {
            Mode::Stream(s) => *s,
            other => panic!("expected the stream subcommand, got {other:?}"),
        }
    }

    #[test]
    fn a_stream_without_a_credential_refuses_to_join() {
        let err = stream_from(&["--jwt-secret", ""])
            .resolve_auth()
            .expect_err("a credential-less invocation must not resolve to a joinable auth mode");
        assert!(
            err.to_string().contains("no room access token"),
            "expected a missing-credential error — got: {err}"
        );
    }

    #[test]
    fn a_secret_defaults_to_a_token_authenticated_url() {
        let options = stream_from(&["--jwt-secret", "secret"]);
        let url = lobby_url_for_attempt(&options, &options.resolve_auth().unwrap()).unwrap();

        assert_eq!(url.path(), "/lobby");
        assert!(url.query().unwrap_or_default().starts_with("token="));
    }

    #[test]
    fn a_secret_wins_over_an_explicit_deprecated_path_request() {
        let options = stream_from(&["--jwt-secret", "secret", "--deprecated-path"]);
        assert!(matches!(
            options.resolve_auth().unwrap(),
            LobbyAuth::Secret { .. }
        ));
    }

    #[test]
    fn the_deprecated_path_is_still_reachable_on_request() {
        let options = stream_from(&["--jwt-secret", "", "--deprecated-path"]);
        let url = lobby_url_for_attempt(&options, &options.resolve_auth().unwrap()).unwrap();

        assert_eq!(url.as_str(), "https://relay.example.com/lobby/cli-1/room-1");
    }
}
