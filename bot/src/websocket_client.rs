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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self as tokio_mpsc, Receiver};
use tokio::task::JoinHandle;
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
type WsSink = futures_util::stream::SplitSink<WsStream, WsMessage>;

pub struct WebSocketClient {
    config: ClientConfig,
    /// Sending half — stored here after connect, moved into packet_sender task.
    write: Option<WsSink>,
    /// Shared reference to the write half for sending Close frames on stop().
    shared_sink: Arc<tokio::sync::Mutex<Option<WsSink>>>,
    /// Channel for forwarding Pong responses from the read half to the write half.
    pong_rx: Option<tokio_mpsc::Receiver<Vec<u8>>>,
    pong_tx: tokio_mpsc::Sender<Vec<u8>>,
    quit: Arc<AtomicBool>,
    /// Handles for spawned tasks so stop() can join them.
    task_handles: Vec<JoinHandle<()>>,
}

impl WebSocketClient {
    pub fn new(config: ClientConfig) -> Self {
        let (pong_tx, pong_rx) = tokio_mpsc::channel(4);
        Self {
            config,
            write: None,
            shared_sink: Arc::new(tokio::sync::Mutex::new(None)),
            pong_rx: Some(pong_rx),
            pong_tx,
            quit: Arc::new(AtomicBool::new(false)),
            task_handles: Vec::new(),
        }
    }

    pub async fn connect(
        &mut self,
        lobby_url: &Url,
        stats: Arc<Mutex<InboundStats>>,
    ) -> anyhow::Result<()> {
        info!("Connecting client {} to {}", self.config.user_id, lobby_url);

        let (ws_stream, _response) = tokio_tungstenite::connect_async(lobby_url.as_str()).await?;
        info!(
            "WebSocket connection established for {}",
            self.config.user_id
        );

        let (write, read) = ws_stream.split();
        self.write = Some(write);

        // Start inbound consumer (drain incoming frames, forward pongs)
        let handle = self.start_inbound_consumer(read, stats.clone());
        self.task_handles.push(handle);

        // Start dedicated 10s stats reporting task (fix #2: separate from read loop)
        let report_handle = self.start_stats_reporter(stats);
        self.task_handles.push(report_handle);

        info!("Inbound consumer started for {}", self.config.user_id);

        Ok(())
    }

    fn start_inbound_consumer(
        &self,
        mut read: futures_util::stream::SplitStream<WsStream>,
        stats: Arc<Mutex<InboundStats>>,
    ) -> JoinHandle<()> {
        let user_id = self.config.user_id.clone();
        let quit = self.quit.clone();
        let pong_tx = self.pong_tx.clone();

        tokio::spawn(async move {
            loop {
                if quit.load(Ordering::Relaxed) {
                    break;
                }

                match read.next().await {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let mut s = stats.lock().unwrap();
                        s.record_packet(&user_id, &data);
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
            let s = stats.lock().unwrap();
            s.report(&user_id);
            info!("Inbound consumer stopped for {}", user_id);
        })
    }

    /// Spawn a dedicated task that reports and resets stats every 10 seconds,
    /// plus evicts stale sender entries. This runs independently of the read
    /// loop so reports fire even when no packets arrive.
    fn start_stats_reporter(&self, stats: Arc<Mutex<InboundStats>>) -> JoinHandle<()> {
        let user_id = self.config.user_id.clone();
        let quit = self.quit.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(10));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                if quit.load(Ordering::Relaxed) {
                    break;
                }
                let mut s = stats.lock().unwrap();
                s.report(&user_id);
                s.evict_stale(Duration::from_secs(60));
                s.reset();
            }
        })
    }

    pub async fn start_packet_sender(&mut self, mut packet_receiver: Receiver<Vec<u8>>) {
        let mut write = self
            .write
            .take()
            .expect("connect() must be called before start_packet_sender()");
        let user_id = self.config.user_id.clone();
        let quit = self.quit.clone();
        let mut pong_rx = self.pong_rx.take();
        let shared_sink = self.shared_sink.clone();

        let handle = tokio::spawn(async move {
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
            // Park the sink in the shared slot so stop() can send a Close frame
            // even after this task exits its send loop.
            *shared_sink.lock().await = Some(write);
            info!("Packet sender stopped for {}", user_id);
        });

        self.task_handles.push(handle);
    }

    pub async fn stop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        info!("Stopping WebSocket client for {}", self.config.user_id);

        // Send a WebSocket Close frame so the server sees a clean disconnect.
        let mut sink_guard = self.shared_sink.lock().await;
        if let Some(ref mut sink) = *sink_guard {
            if let Err(e) = sink.send(WsMessage::Close(None)).await {
                debug!(
                    "Could not send WS Close for {}: {} (may already be closed)",
                    self.config.user_id, e
                );
            }
        }
        drop(sink_guard);

        // Join/abort all spawned tasks with a timeout.
        let handles: Vec<JoinHandle<()>> = self.task_handles.drain(..).collect();
        for handle in handles {
            let timeout_result = tokio::time::timeout(Duration::from_secs(5), handle).await;
            match timeout_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    debug!("Task join error for {}: {}", self.config.user_id, e);
                }
                Err(_) => {
                    warn!(
                        "Task did not finish within 5s for {}, aborting",
                        self.config.user_id
                    );
                }
            }
        }

        info!("WebSocket client stopped for {}", self.config.user_id);
    }
}

/// Build the heartbeat protobuf packet bytes (shared helper).
pub fn build_heartbeat_packet(
    user_id: &str,
    audio_enabled: bool,
    video_enabled: bool,
    is_speaking: bool,
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
            is_speaking,
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
    is_speaking: Arc<AtomicBool>,
) {
    static HB_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        loop {
            if quit.load(Ordering::Relaxed) {
                break;
            }
            interval.tick().await;
            let speaking = is_speaking.load(Ordering::Relaxed);
            match build_heartbeat_packet(&user_id, audio_enabled, video_enabled, speaking) {
                Ok(data) => {
                    if let Err(_e) = packet_sender.try_send(data) {
                        let count = HB_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if count % 100 == 1 {
                            warn!(
                                "Dropped heartbeat packets due to full send channel (total: {})",
                                count,
                            );
                        }
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
