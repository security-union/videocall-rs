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
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket, TransportType};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::url_log::strip_query_for_log;

use crate::inbound_stats::InboundStats;

use crate::config::{ClientConfig, Transport};
#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;
use crate::transport::ClosedSignal;
use crate::transport::{InboundHook, MediaTypeLabel, OutboundFrame, OutboundFrameSender};

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, WsMessage>;

/// Small burst buffer for Ping -> Pong forwarding.
///
/// A bounded queue prevents the read half from ever blocking on the writer
/// task, while 32 slots easily covers transient scheduler hiccups without
/// retaining unbounded Ping payloads.
const PONG_QUEUE_CAPACITY: usize = 32;

pub struct WebSocketClient {
    config: ClientConfig,
    /// Sending half — stored here after connect, moved into packet_sender task.
    write: Option<WsSink>,
    /// Shared reference to the write half for sending Close frames on stop().
    shared_sink: Arc<tokio::sync::Mutex<Option<WsSink>>>,
    /// Channel for forwarding Pong responses from the read half to the write half.
    pong_rx: Option<tokio_mpsc::Receiver<Vec<u8>>>,
    pong_tx: tokio_mpsc::Sender<Vec<u8>>,
    #[cfg(feature = "metrics")]
    metrics: Option<Arc<BotMetrics>>,
    quit: Arc<AtomicBool>,
    closed: ClosedSignal,
    /// Handles for spawned tasks so stop() can join them.
    task_handles: Vec<JoinHandle<()>>,
}

impl WebSocketClient {
    pub fn new(
        config: ClientConfig,
        #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
    ) -> Self {
        let (pong_tx, pong_rx) = tokio_mpsc::channel(PONG_QUEUE_CAPACITY);
        Self {
            config,
            write: None,
            shared_sink: Arc::new(tokio::sync::Mutex::new(None)),
            pong_rx: Some(pong_rx),
            pong_tx,
            #[cfg(feature = "metrics")]
            metrics,
            quit: Arc::new(AtomicBool::new(false)),
            closed: ClosedSignal::default(),
            task_handles: Vec::new(),
        }
    }

    pub fn closed(&self) -> &ClosedSignal {
        &self.closed
    }

    pub async fn connect(
        &mut self,
        lobby_url: &Url,
        stats: Arc<Mutex<InboundStats>>,
        inbound_hook: Option<InboundHook>,
    ) -> anyhow::Result<()> {
        info!(
            "Connecting client {} to {}",
            self.config.user_id,
            strip_query_for_log(lobby_url.as_str())
        );

        let (ws_stream, _response) = tokio_tungstenite::connect_async(lobby_url.as_str()).await?;
        info!(
            "WebSocket connection established for {}",
            self.config.user_id
        );

        let (write, read) = ws_stream.split();
        self.write = Some(write);

        // Start inbound consumer (drain incoming frames, forward pongs)
        let handle = self.start_inbound_consumer(read, stats.clone(), inbound_hook);
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
        inbound_hook: Option<InboundHook>,
    ) -> JoinHandle<()> {
        let user_id = self.config.user_id.clone();
        #[cfg(feature = "metrics")]
        let meeting_id = self.config.meeting_id.clone();
        let quit = self.quit.clone();
        let closed = self.closed.clone();
        let pong_tx = self.pong_tx.clone();
        #[cfg(feature = "metrics")]
        let metrics = self.metrics.clone();

        tokio::spawn(async move {
            loop {
                if quit.load(Ordering::Relaxed) {
                    break;
                }

                match read.next().await {
                    Some(Ok(WsMessage::Binary(data))) => match &inbound_hook {
                        Some(h) => h(data),
                        None => {
                            let mut s = stats.lock().unwrap();
                            s.record_packet(&user_id, &data);
                        }
                    },
                    Some(Ok(WsMessage::Ping(data))) => {
                        debug!("Received WS ping for {}", user_id);
                        if let Err(e) = pong_tx.try_send(data) {
                            static PONG_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                            let count = PONG_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                            debug!(
                                "[{}] dropping queued WS pong response (count={}): {}",
                                user_id, count, e
                            );
                            if count.is_multiple_of(100) || count == 1 {
                                warn!(
                                    "[{}] WS pong queue overflow/backpressure; dropped {} queued pong responses so far",
                                    user_id, count
                                );
                            }
                            #[cfg(feature = "metrics")]
                            if let Some(ref m) = metrics {
                                let reason = match e {
                                    tokio_mpsc::error::TrySendError::Full(_) => "queue_full",
                                    tokio_mpsc::error::TrySendError::Closed(_) => "channel_closed",
                                };
                                m.websocket_pong_drops_total
                                    .with_label_values(&[
                                        user_id.as_str(),
                                        meeting_id.as_str(),
                                        reason,
                                    ])
                                    .inc();
                            }
                        }
                    }
                    Some(Ok(WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_))) => {
                        info!("Server closed connection for {}", user_id);
                        closed.report(&quit, "relay closed the WebSocket");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket read error for {}: {}", user_id, e);
                        closed.report(&quit, format!("WebSocket read error: {e}"));
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended for {}", user_id);
                        closed.report(&quit, "WebSocket stream ended");
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
                s.evict_stale(crate::inbound_stats::PEER_SILENCE_EVICT);
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
        let closed = self.closed.clone();
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
                            closed.report(&quit, format!("WebSocket write error: {e}"));
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
                                closed.report(&quit, format!("WebSocket write error: {e}"));
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

/// Build the heartbeat protobuf packet bytes (shared helper). The metadata
/// matches the browser's (`videocall-client/src/connection/connection.rs`
/// `build_heartbeat_packet`): media flags, speaking state and transport type.
pub fn build_heartbeat_packet(
    config: &ClientConfig,
    transport: &Transport,
    is_speaking: bool,
) -> anyhow::Result<Vec<u8>> {
    let user_id = config.user_id.as_str();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis();

    let heartbeat = MediaPacket {
        media_type: MediaType::HEARTBEAT.into(),
        user_id: user_id.as_bytes().to_vec(),
        timestamp: now_ms as f64,
        heartbeat_metadata: Some(HeartbeatMetadata {
            video_enabled: config.enable_video,
            audio_enabled: config.enable_audio,
            is_speaking,
            transport_type: match transport {
                Transport::WebSocket => TransportType::TRANSPORT_WEBSOCKET,
                Transport::WebTransport => TransportType::TRANSPORT_WEBTRANSPORT,
            }
            .into(),
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

/// How often the heartbeat loops check for a speaking change.
pub const HEARTBEAT_POLL: Duration = Duration::from_millis(100);

/// Whether a heartbeat is due: none sent yet, the keepalive interval elapsed,
/// or the speaking state changed (the browser sends immediately on that edge,
/// `videocall-client/src/connection/connection.rs` `set_speaking`).
pub fn heartbeat_due(
    since_last: Option<Duration>,
    interval: Duration,
    speaking_changed: bool,
) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => speaking_changed || elapsed >= interval,
    }
}

/// Tracks the last heartbeat and speaking state for one heartbeat loop.
#[derive(Default)]
pub struct HeartbeatClock {
    last_sent: Option<std::time::Instant>,
    last_speaking: bool,
}

impl HeartbeatClock {
    /// Returns `Some(speaking)` when a heartbeat should go out now, and records it.
    pub fn poll(&mut self, interval: Duration, speaking: bool) -> Option<bool> {
        let now = std::time::Instant::now();
        let since = self.last_sent.map(|t| now.duration_since(t));
        let changed = self.last_sent.is_some() && speaking != self.last_speaking;
        if heartbeat_due(since, interval, changed) {
            self.last_sent = Some(now);
            self.last_speaking = speaking;
            Some(speaking)
        } else {
            None
        }
    }
}

/// Spawn a heartbeat producer that feeds packets into the shared mpsc channel.
pub fn spawn_heartbeat_producer(
    config: ClientConfig,
    packet_sender: OutboundFrameSender,
    quit: Arc<AtomicBool>,
    is_speaking: Arc<AtomicBool>,
) {
    static HB_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

    tokio::spawn(async move {
        let heartbeat_interval = config.heartbeat_interval;
        let user_id = config.user_id.clone();
        let mut poll = time::interval(HEARTBEAT_POLL.min(heartbeat_interval));
        let mut clock = HeartbeatClock::default();
        loop {
            if quit.load(Ordering::Relaxed) {
                break;
            }
            poll.tick().await;
            let Some(speaking) =
                clock.poll(heartbeat_interval, is_speaking.load(Ordering::Relaxed))
            else {
                continue;
            };
            match build_heartbeat_packet(&config, &Transport::WebSocket, speaking) {
                Ok(data) => {
                    let frame = OutboundFrame::new(MediaTypeLabel::Heartbeat, data);
                    if let Err(_e) = packet_sender.try_send(frame) {
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

#[cfg(test)]
mod close_tests {
    use super::WebSocketClient;
    use crate::config::{ClientConfig, Role};
    use crate::inbound_stats::InboundStats;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    /// A relay stand-in that accepts one WebSocket and closes it after `after`.
    async fn relay_that_closes(after: Duration) -> url::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Reading answers a client Close; otherwise close after `after`.
            let _ =
                tokio::time::timeout(after, async { while let Some(Ok(_)) = ws.next().await {} })
                    .await;
            let _ = ws.send(Message::Close(None)).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        url::Url::parse(&format!("ws://{addr}/lobby")).unwrap()
    }

    fn client() -> WebSocketClient {
        WebSocketClient::new(
            ClientConfig::for_role("u".into(), "m".into(), Role::Viewer, Duration::from_secs(5)),
            #[cfg(feature = "metrics")]
            None,
        )
    }

    #[tokio::test]
    async fn a_relay_close_is_reported_to_the_client() {
        let url = relay_that_closes(Duration::from_millis(50)).await;
        let mut c = client();
        let stats = Arc::new(Mutex::new(InboundStats::default()));
        c.connect(&url, stats, None).await.unwrap();
        let mut closed = c.closed().subscribe();
        let reason = tokio::time::timeout(
            Duration::from_secs(5),
            crate::transport::wait_closed(&mut closed),
        )
        .await
        .expect("the close must be signalled");
        assert!(reason.contains("closed"), "{}", reason);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_error_is_reported_as_the_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.send(Message::Binary(vec![0])).await;
        });
        let url = url::Url::parse(&format!("ws://{addr}/lobby")).unwrap();
        // The hook holds the reader so only the writer can see the drop.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let hook: crate::transport::InboundHook = Arc::new(move |_| {
            tokio::task::block_in_place(|| {
                let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
            });
        });
        let mut c = client();
        let stats = Arc::new(Mutex::new(InboundStats::default()));
        c.connect(&url, stats, Some(hook)).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        c.start_packet_sender(rx).await;
        let mut closed = c.closed().subscribe();
        let reason = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let _ = tx.try_send(vec![0; 64]);
                tokio::select! {
                    r = crate::transport::wait_closed(&mut closed) => break r,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        })
        .await
        .expect("the write error must be signalled");
        drop(release);
        assert!(reason.starts_with("WebSocket write error"), "{}", reason);
    }

    #[tokio::test]
    async fn a_local_stop_is_not_a_relay_close() {
        let url = relay_that_closes(Duration::from_secs(5)).await;
        let mut c = client();
        let stats = Arc::new(Mutex::new(InboundStats::default()));
        c.connect(&url, stats, None).await.unwrap();
        let closed = c.closed().subscribe();
        c.stop().await;
        assert_eq!(*closed.borrow(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{build_heartbeat_packet, heartbeat_due, HeartbeatClock};
    use crate::config::{ClientConfig, Role, Transport};
    use protobuf::Message;
    use std::time::Duration;
    use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket, TransportType};
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    fn heartbeat_for(role: Role, transport: Transport) -> HeartbeatMetadata {
        let config = ClientConfig::for_role("u".into(), "m".into(), role, FIVE_S);
        let bytes = build_heartbeat_packet(&config, &transport, false).unwrap();
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let media = MediaPacket::parse_from_bytes(&wrapper.data).unwrap();
        media.heartbeat_metadata.unwrap()
    }

    #[test]
    fn heartbeat_carries_the_role_media_flags_and_transport_like_the_browser() {
        let camera = heartbeat_for(Role::Camera, Transport::WebSocket);
        assert!(camera.video_enabled && !camera.audio_enabled);
        assert_eq!(
            camera.transport_type.enum_value(),
            Ok(TransportType::TRANSPORT_WEBSOCKET)
        );
        let presenter = heartbeat_for(Role::Presenter, Transport::WebTransport);
        assert!(presenter.video_enabled && presenter.audio_enabled);
        assert_eq!(
            presenter.transport_type.enum_value(),
            Ok(TransportType::TRANSPORT_WEBTRANSPORT)
        );
        let viewer = heartbeat_for(Role::Viewer, Transport::WebSocket);
        assert!(!viewer.video_enabled && !viewer.audio_enabled);
    }

    const FIVE_S: Duration = Duration::from_secs(5);

    #[test]
    fn heartbeat_is_due_first_on_keepalive_and_on_speaking_change() {
        assert!(heartbeat_due(None, FIVE_S, false));
        assert!(!heartbeat_due(Some(Duration::from_secs(1)), FIVE_S, false));
        assert!(heartbeat_due(Some(Duration::from_secs(1)), FIVE_S, true));
        assert!(heartbeat_due(Some(FIVE_S), FIVE_S, false));
    }

    #[test]
    fn heartbeat_clock_sends_once_then_waits_for_the_keepalive_or_a_speaking_edge() {
        let mut clock = HeartbeatClock::default();
        assert_eq!(clock.poll(FIVE_S, false), Some(false));
        assert_eq!(clock.poll(FIVE_S, false), None, "keepalive not elapsed");
        assert_eq!(clock.poll(FIVE_S, true), Some(true), "speaking edge");
        assert_eq!(clock.poll(FIVE_S, true), None);
    }
}
