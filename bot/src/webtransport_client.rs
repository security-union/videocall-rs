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

use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Receiver;
use tokio::time;
use tracing::{debug, info, warn};
use url::Url;
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use web_transport_quinn::{ClientBuilder, Session};

use crate::config::ClientConfig;

pub struct WebTransportClient {
    config: ClientConfig,
    session: Option<Session>,
    quit: Arc<AtomicBool>,
}

impl WebTransportClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            session: None,
            quit: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn connect(&mut self, server_url: &Url, insecure: bool) -> anyhow::Result<()> {
        info!(
            "Connecting client {} to {}",
            self.config.user_id, server_url
        );

        // Create WebTransport client (same logic as webtranscat)
        let client = if insecure {
            warn!("Certificate verification disabled (--insecure)");
            // SAFETY: This is intentionally insecure for testing purposes
            unsafe { ClientBuilder::new().with_no_certificate_verification()? }
        } else {
            // Use default secure configuration with system certificates
            ClientBuilder::new().with_system_roots()?
        };

        // Construct full URL with lobby path
        let full_url = format!(
            "{}/lobby/{}/{}",
            server_url.as_str().trim_end_matches('/'),
            self.config.user_id,
            self.config.meeting_id
        );
        let connection_url = Url::parse(&full_url)?;

        info!("Connecting to {}", connection_url);
        let session = client.connect(connection_url).await?;
        info!(
            "WebTransport session established for {}",
            self.config.user_id
        );

        self.session = Some(session);
        info!(
            "WebTransport session established for {}",
            self.config.user_id
        );

        // Send connection packet
        self.send_connection_packet().await?;

        // Start heartbeat
        self.start_heartbeat().await;
        info!("Heartbeat started for {}", self.config.user_id);

        Ok(())
    }

    async fn send_connection_packet(&self) -> anyhow::Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.config.meeting_id.clone(),
            ..Default::default()
        };

        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            email: self.config.user_id.clone(),
            data: connection_packet.write_to_bytes()?,
            ..Default::default()
        };

        self.send_packet(packet.write_to_bytes()?).await?;
        info!("Sent connection packet for {}", self.config.user_id);
        Ok(())
    }

    async fn start_heartbeat(&self) {
        if let Some(session) = &self.session {
            let session = session.clone();
            let user_id = self.config.user_id.clone();
            let video_enabled = self.config.enable_video; // Get actual video config
            let quit = self.quit.clone();

            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(1));

                loop {
                    if quit.load(Ordering::Relaxed) {
                        break;
                    }

                    interval.tick().await;

                    // Use exact same timestamp calculation as videocall-cli
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("Time went backwards")
                        .as_millis();

                    let heartbeat = MediaPacket {
                        media_type: MediaType::HEARTBEAT.into(),
                        email: user_id.clone(),
                        timestamp: now_ms as f64,
                        heartbeat_metadata: Some(HeartbeatMetadata {
                            video_enabled,
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    };

                    let packet = PacketWrapper {
                        email: user_id.clone(),
                        packet_type: PacketType::MEDIA.into(),
                        data: heartbeat.write_to_bytes().unwrap(),
                        ..Default::default()
                    };

                    if let Err(e) =
                        Self::send_via_session(&session, packet.write_to_bytes().unwrap()).await
                    {
                        warn!("Failed to send heartbeat for {}: {}", user_id, e);
                    } else {
                        debug!("Sent heartbeat for {}", user_id);
                    }
                }
            });
        }
    }

    pub async fn send_packet(&self, data: Vec<u8>) -> anyhow::Result<()> {
        if let Some(session) = &self.session {
            Self::send_via_session(session, data).await
        } else {
            Err(anyhow::anyhow!("No WebTransport session available"))
        }
    }

    async fn send_via_session(session: &Session, data: Vec<u8>) -> anyhow::Result<()> {
        let mut stream = session.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish()?; // Remove .await as this is not async
        Ok(())
    }

    pub async fn start_packet_sender(&self, mut packet_receiver: Receiver<Vec<u8>>) {
        if let Some(session) = &self.session {
            let session = session.clone();
            let user_id = self.config.user_id.clone();
            let quit = self.quit.clone();

            tokio::spawn(async move {
                while let Some(packet_data) = packet_receiver.recv().await {
                    if quit.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(e) = Self::send_via_session(&session, packet_data).await {
                        warn!("Failed to send media packet for {}: {}", user_id, e);
                    }
                }
                info!("Packet sender stopped for {}", user_id);
            });
        }
    }

    pub fn stop(&self) {
        self.quit.store(true, Ordering::Relaxed);
        info!("Stopping WebTransport client for {}", self.config.user_id);
    }
}
