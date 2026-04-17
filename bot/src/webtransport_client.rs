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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time;
use tracing::{debug, info, warn};
use url::Url;
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use web_transport_quinn::{ClientBuilder, Session};

use crate::config::ClientConfig;
use crate::inbound_stats::InboundStats;
use crate::websocket_client::build_heartbeat_packet;

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

    pub async fn connect(
        &mut self,
        lobby_url: &Url,
        insecure: bool,
        stats: Arc<Mutex<InboundStats>>,
    ) -> anyhow::Result<()> {
        info!("Connecting client {} to {}", self.config.user_id, lobby_url);

        let client = if insecure {
            warn!("Certificate verification disabled (--insecure)");
            // SAFETY: This is intentionally insecure for testing purposes
            unsafe { ClientBuilder::new().with_no_certificate_verification()? }
        } else {
            ClientBuilder::new().with_system_roots()?
        };

        info!("Connecting to {}", lobby_url);
        let session = client.connect(lobby_url.clone()).await?;
        info!(
            "WebTransport session established for {}",
            self.config.user_id
        );

        self.session = Some(session);

        // Send connection packet
        self.send_connection_packet().await?;

        // Start heartbeat
        self.start_heartbeat().await;
        info!("Heartbeat started for {}", self.config.user_id);

        // Start inbound consumer to avoid being a slow consumer
        self.start_inbound_consumer(stats).await;
        info!("Inbound consumer started for {}", self.config.user_id);

        Ok(())
    }

    async fn send_connection_packet(&self) -> anyhow::Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.config.meeting_id.clone(),
            ..Default::default()
        };

        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            user_id: self.config.user_id.clone().into_bytes(),
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
            let video_enabled = self.config.enable_video;
            let audio_enabled = self.config.enable_audio;
            let quit = self.quit.clone();

            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(1));

                loop {
                    if quit.load(Ordering::Relaxed) {
                        break;
                    }

                    interval.tick().await;

                    match build_heartbeat_packet(&user_id, audio_enabled, video_enabled) {
                        Ok(data) => {
                            if let Err(e) = Self::send_via_session(&session, data).await {
                                warn!("Failed to send heartbeat for {}: {}", user_id, e);
                            } else {
                                debug!("Sent heartbeat for {}", user_id);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to build heartbeat for {}: {}", user_id, e);
                        }
                    }
                }
            });
        }
    }

    /// Start a task to consume all inbound unistreams and track quality stats
    async fn start_inbound_consumer(&self, stats: Arc<Mutex<InboundStats>>) {
        if let Some(session) = &self.session {
            let session = session.clone();
            let user_id = self.config.user_id.clone();
            let quit = self.quit.clone();

            let stats_clone = stats.clone();
            let user_id_report = user_id.clone();
            let quit_report = quit.clone();

            // Periodic reporter task — mirrors the WS pattern: report, evict stale, reset.
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(10));
                interval.tick().await; // skip first immediate tick
                loop {
                    interval.tick().await;
                    if quit_report.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut s = stats_clone.lock().unwrap();
                    s.report(&user_id_report);
                    s.evict_stale(Duration::from_secs(60));
                    s.reset();
                }
            });

            tokio::spawn(async move {
                loop {
                    if quit.load(Ordering::Relaxed) {
                        break;
                    }

                    match session.accept_uni().await {
                        Ok(mut stream) => {
                            let user_id = user_id.clone();
                            let stats = stats.clone();
                            tokio::spawn(async move {
                                match stream.read_to_end(usize::MAX).await {
                                    Ok(data) => {
                                        let mut s = stats.lock().unwrap();
                                        s.record_packet(&user_id, &data);
                                    }
                                    Err(e) => {
                                        debug!(
                                            "Error reading inbound unistream for {}: {}",
                                            user_id, e
                                        );
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            debug!("Inbound consumer ended for {}: {}", user_id, e);
                            break;
                        }
                    }
                }
                let s = stats.lock().unwrap();
                s.report(&user_id);
                info!("Inbound consumer stopped for {}", user_id);
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
        stream.finish()?;
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
