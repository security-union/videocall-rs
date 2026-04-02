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

//! WebSocket transport client for the synthetic bot.
//!
//! Connects via `wss://host/lobby?token=<jwt>` (or the deprecated path-based
//! URL when no JWT secret is configured). Sends protobuf `PacketWrapper`
//! messages as binary WebSocket frames — identical wire format to the browser
//! client.

use futures_util::{SinkExt, StreamExt};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self as tokio_mpsc, Receiver};
use tokio::time;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};
use url::Url;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use crate::inbound_stats::InboundStats;

use crate::config::ClientConfig;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct WebSocketClient {
    config: ClientConfig,
    /// Sending half — stored here after connect, moved into packet_sender task.
    write: Option<futures_util::stream::SplitSink<WsStream, WsMessage>>,
    /// Channel for forwarding Pong responses from the read half to the write half.
    pong_rx: Option<tokio_mpsc::Receiver<Vec<u8>>>,
    pong_tx: tokio_mpsc::Sender<Vec<u8>>,
    quit: Arc<AtomicBool>,
}

impl WebSocketClient {
    pub fn new(config: ClientConfig) -> Self {
        let (pong_tx, pong_rx) = tokio_mpsc::channel(4);
        Self {
            config,
            write: None,
            pong_rx: Some(pong_rx),
            pong_tx,
            quit: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn connect(&mut self, lobby_url: &Url) -> anyhow::Result<()> {
        info!("Connecting client {} to {}", self.config.user_id, lobby_url);

        let (ws_stream, _response) = tokio_tungstenite::connect_async(lobby_url.as_str()).await?;
        info!(
            "WebSocket connection established for {}",
            self.config.user_id
        );

        let (write, read) = ws_stream.split();
        self.write = Some(write);

        // Start inbound consumer (drain incoming frames, forward pongs)
        self.start_inbound_consumer(read).await;
        info!("Inbound consumer started for {}", self.config.user_id);

        Ok(())
    }

    async fn start_inbound_consumer(&self, mut read: futures_util::stream::SplitStream<WsStream>) {
        let user_id = self.config.user_id.clone();
        let quit = self.quit.clone();
        let pong_tx = self.pong_tx.clone();

        tokio::spawn(async move {
            let mut stats = InboundStats::default();
            let stats_interval = Duration::from_secs(10);
            let mut next_report = time::Instant::now() + stats_interval;

            loop {
                if quit.load(Ordering::Relaxed) {
                    break;
                }

                match read.next().await {
                    Some(Ok(WsMessage::Binary(data))) => {
                        stats.record_packet(&user_id, &data);

                        if time::Instant::now() >= next_report {
                            stats.report(&user_id);
                            stats.reset();
                            next_report = time::Instant::now() + stats_interval;
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        debug!("Received WS ping for {}", user_id);
                        let _ = pong_tx.try_send(data);
                    }
                    Some(Ok(WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_))) => {
                        info!("Server closed connection for {}", user_id);
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket read error for {}: {}", user_id, e);
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended for {}", user_id);
                        break;
                    }
                    _ => {}
                }
            }
            stats.report(&user_id);
            info!("Inbound consumer stopped for {}", user_id);
        });
    }

    pub async fn start_packet_sender(&mut self, mut packet_receiver: Receiver<Vec<u8>>) {
        let mut write = self
            .write
            .take()
            .expect("connect() must be called before start_packet_sender()");
        let user_id = self.config.user_id.clone();
        let quit = self.quit.clone();
        let mut pong_rx = self.pong_rx.take();

        tokio::spawn(async move {
            loop {
                if quit.load(Ordering::Relaxed) {
                    break;
                }
                tokio::select! {
                    packet = packet_receiver.recv() => {
                        let Some(packet_data) = packet else { break };
                        if let Err(e) = write.send(WsMessage::Binary(packet_data)).await {
                            warn!("Failed to send WS packet for {}: {}", user_id, e);
                            break;
                        }
                    }
                    pong = async {
                        match pong_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(data) = pong {
                            if let Err(e) = write.send(WsMessage::Pong(data)).await {
                                warn!("Failed to send WS pong for {}: {}", user_id, e);
                                break;
                            }
                        }
                    }
                }
            }
            info!("Packet sender stopped for {}", user_id);
        });
    }

    pub fn stop(&self) {
        self.quit.store(true, Ordering::Relaxed);
        info!("Stopping WebSocket client for {}", self.config.user_id);
    }
}

/// Build the heartbeat protobuf packet bytes (shared helper).
pub fn build_heartbeat_packet(
    user_id: &str,
    audio_enabled: bool,
    video_enabled: bool,
) -> anyhow::Result<Vec<u8>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis();

    let heartbeat = MediaPacket {
        media_type: MediaType::HEARTBEAT.into(),
        user_id: user_id.as_bytes().to_vec(),
        timestamp: now_ms as f64,
        heartbeat_metadata: Some(HeartbeatMetadata {
            video_enabled,
            audio_enabled,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };

    let packet = PacketWrapper {
        user_id: user_id.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        data: heartbeat.write_to_bytes()?,
        ..Default::default()
    };

    Ok(packet.write_to_bytes()?)
}

/// Spawn a heartbeat producer that feeds packets into the shared mpsc channel.
pub fn spawn_heartbeat_producer(
    user_id: String,
    audio_enabled: bool,
    video_enabled: bool,
    packet_sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    quit: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        loop {
            if quit.load(Ordering::Relaxed) {
                break;
            }
            interval.tick().await;
            match build_heartbeat_packet(&user_id, audio_enabled, video_enabled) {
                Ok(data) => {
                    if let Err(e) = packet_sender.try_send(data) {
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
        info!("Heartbeat producer stopped for {}", user_id);
    });
}
