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
use secrecy::ExposeSecret;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::mpsc::{self, Sender},
    time::{self, Duration},
};
use tracing::info;
use videocall_types::protos::{
    connection_packet::ConnectionPacket,
    media_packet::{media_packet::MediaType, HeartbeatMetadata, MediaPacket},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};

use crate::auth::Credentials;
use crate::cli_args::Stream;
use crate::room;

use super::camera_synk::CameraSynk;

pub struct WebTransportClient {
    options: Stream,
    creds: Credentials,
    user_id: String,
    sender: Option<Sender<Vec<u8>>>,
}

impl WebTransportClient {
    pub fn new(options: Stream, creds: Credentials, user_id: String) -> Self {
        Self {
            options,
            creds,
            user_id,
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
                if let Err(e) = Self::send(&session, data).await {
                    tracing::error!("Failed to send heartbeat: {}", e);
                }
            }
        });
    }
}

pub fn lobby_url(relay: &url::Url, room_token: &str) -> url::Url {
    let mut url = relay.clone();
    url.set_path("/lobby");
    url.set_query(None);
    url.query_pairs_mut().append_pair("token", room_token);
    url
}

async fn connect_to_server(
    options: &Stream,
    creds: &Credentials,
) -> anyhow::Result<web_transport_quinn::Session> {
    loop {
        info!("Attempting to connect to {}", options.url);

        // A fresh room token for every attempt: they expire after 300 s.
        let token = match room::room_token(creds, &options.meeting_id).await {
            Ok(token) => token,
            Err(e) if room::is_transient(&e) => {
                tracing::error!("Couldn't get a room token: {e}. Retrying in 5 seconds...");
                time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let url = lobby_url(&options.url, token.expose_secret());

        // Create WebTransport client using 0.7.3 API (same pattern as bot)
        let client = if options.insecure_skip_verify {
            info!("WARNING: Skipping TLS certificate verification - connection is insecure!");
            unsafe { web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()? }
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
        let session = connect_to_server(&self.options, &self.creds).await?;
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
    use super::*;

    #[test]
    fn lobby_url_carries_only_the_token() {
        let relay = url::Url::parse("https://relay.example:4433/old/path?x=1").unwrap();
        let url = lobby_url(&relay, "a.b+c");
        assert_eq!(
            url.as_str(),
            "https://relay.example:4433/lobby?token=a.b%2Bc"
        );
    }
}
