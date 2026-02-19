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

//! Native (non-WASM) video call client.
//!
//! Provides connection lifecycle, heartbeat, and packet I/O for native applications
//! such as bots, CLI tools, and embedded devices.  Media capture and encoding are
//! left to the caller — this client only handles the **protocol layer**.
//!
//! # Example
//!
//! ```no_run
//! use videocall_client::{NativeVideoCallClient, NativeClientOptions};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let client = NativeVideoCallClient::new(NativeClientOptions {
//!         userid: "bot-001".into(),
//!         meeting_id: "room-42".into(),
//!         webtransport_url: "https://server:4433/lobby/bot-001/room-42".into(),
//!         insecure: false,
//!         on_inbound_packet: Box::new(|_pkt| { /* handle incoming */ }),
//!         on_connected: Box::new(|| println!("Connected!")),
//!         on_disconnected: Box::new(|err| eprintln!("Disconnected: {err}")),
//!         enable_e2ee: false,
//!     });
//!
//!     let mut client = client;
//!     client.connect().await?;
//!     // ... send media packets via client.send_packet(wrapper) ...
//!     client.disconnect()?;
//!     Ok(())
//! }
//! ```

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use protobuf::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use videocall_types::protos::connection_packet::ConnectionPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::{HeartbeatMetadata, MediaPacket};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use crate::platform;

/// Configuration for [`NativeVideoCallClient`].
pub struct NativeClientOptions {
    /// The user ID for this client (appears as peer ID to other participants).
    pub userid: String,

    /// The meeting / room ID to join.
    pub meeting_id: String,

    /// Full WebTransport URL including the lobby path,
    /// e.g. `"https://server:4433/lobby/{userid}/{meeting_id}"`.
    pub webtransport_url: String,

    /// If `true`, skip TLS certificate verification (testing only!).
    pub insecure: bool,

    /// Called when an inbound packet arrives from the server.
    pub on_inbound_packet: Box<dyn Fn(PacketWrapper) + Send + Sync>,

    /// Called when the connection is established.
    pub on_connected: Box<dyn Fn() + Send + Sync>,

    /// Called when the connection is lost, with an error description.
    pub on_disconnected: Box<dyn Fn(String) + Send + Sync>,

    /// Whether to enable end-to-end encryption.
    pub enable_e2ee: bool,
}

/// A native videocall client that handles connection, heartbeat, and packet I/O.
///
/// This client does **not** handle media encoding/decoding — it only manages the
/// protocol layer (connection lifecycle, heartbeat, packet send/receive).
/// Media producers should construct `PacketWrapper` messages and call
/// [`send_packet`](Self::send_packet).
pub struct NativeVideoCallClient {
    options: NativeClientOptions,
    session: Option<web_transport_quinn::Session>,
    packet_tx: Option<mpsc::Sender<Vec<u8>>>,
    quit: Arc<AtomicBool>,
    video_enabled: Arc<AtomicBool>,
    audio_enabled: Arc<AtomicBool>,
    screen_enabled: Arc<AtomicBool>,
}

impl NativeVideoCallClient {
    /// Create a new native client with the given options.
    ///
    /// The client is **not** connected yet — call [`connect()`](Self::connect) to
    /// establish the WebTransport session.
    pub fn new(options: NativeClientOptions) -> Self {
        Self {
            options,
            session: None,
            packet_tx: None,
            quit: Arc::new(AtomicBool::new(false)),
            video_enabled: Arc::new(AtomicBool::new(false)),
            audio_enabled: Arc::new(AtomicBool::new(false)),
            screen_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Connect to the WebTransport server.
    ///
    /// This establishes the QUIC session, sends the initial connection packet,
    /// and starts the heartbeat timer.
    pub async fn connect(&mut self) -> Result<()> {
        info!(
            "NativeVideoCallClient connecting as '{}' to '{}'",
            self.options.userid, self.options.webtransport_url
        );

        let client = if self.options.insecure {
            warn!("TLS certificate verification disabled (insecure mode)");
            unsafe {
                web_transport_quinn::ClientBuilder::new().with_no_certificate_verification()?
            }
        } else {
            web_transport_quinn::ClientBuilder::new().with_system_roots()?
        };

        let url = url::Url::parse(&self.options.webtransport_url)?;
        let session = client.connect(url).await?;
        info!(
            "WebTransport session established for '{}'",
            self.options.userid
        );

        self.session = Some(session.clone());

        // Set up the send channel
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
        self.packet_tx = Some(tx);

        // Spawn the send loop
        let session_send = session.clone();
        let quit_send = self.quit.clone();
        let user_id_send = self.options.userid.clone();
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                if quit_send.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = Self::send_via_session(&session_send, data).await {
                    warn!(
                        "Failed to send packet for {}: {}",
                        user_id_send, e
                    );
                }
            }
            debug!("Send loop stopped for {}", user_id_send);
        });

        // Spawn inbound consumer
        self.start_inbound_consumer(session.clone()).await;

        // Send connection packet
        self.send_connection_packet().await?;

        // Start heartbeat
        self.start_heartbeat(session.clone());

        // Notify connected
        (self.options.on_connected)();

        info!(
            "NativeVideoCallClient fully connected for '{}'",
            self.options.userid
        );

        Ok(())
    }

    /// Send a pre-built `PacketWrapper` to the server.
    ///
    /// Media producers should construct their packets and call this method.
    /// The packet is queued and sent asynchronously.
    pub fn send_packet(&self, packet: PacketWrapper) -> Result<()> {
        let data = packet
            .write_to_bytes()
            .map_err(|e| anyhow!("Failed to serialize packet: {e}"))?;
        self.send_raw(data)
    }

    /// Send raw pre-serialized bytes to the server.
    ///
    /// Useful when you've already serialized the `PacketWrapper` yourself.
    pub fn send_raw(&self, data: Vec<u8>) -> Result<()> {
        if let Some(tx) = &self.packet_tx {
            tx.try_send(data)
                .map_err(|e| anyhow!("Send channel error: {e}"))
        } else {
            Err(anyhow!("Not connected"))
        }
    }

    /// Whether the client is currently connected.
    pub fn is_connected(&self) -> bool {
        self.session.is_some() && !self.quit.load(Ordering::Relaxed)
    }

    /// Disconnect from the server and stop all background tasks.
    pub fn disconnect(&mut self) -> Result<()> {
        info!(
            "Disconnecting NativeVideoCallClient for '{}'",
            self.options.userid
        );
        self.quit.store(true, Ordering::Relaxed);
        self.session = None;
        self.packet_tx = None;
        Ok(())
    }

    /// Set whether this client is sending video (reflected in heartbeat metadata).
    pub fn set_video_enabled(&self, enabled: bool) {
        self.video_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Set whether this client is sending audio (reflected in heartbeat metadata).
    pub fn set_audio_enabled(&self, enabled: bool) {
        self.audio_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Set whether this client is sharing screen (reflected in heartbeat metadata).
    pub fn set_screen_enabled(&self, enabled: bool) {
        self.screen_enabled.store(enabled, Ordering::Relaxed);
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn send_via_session(
        session: &web_transport_quinn::Session,
        data: Vec<u8>,
    ) -> Result<()> {
        let mut stream = session.open_uni().await?;
        stream.write_all(&data).await?;
        stream.finish()?;
        Ok(())
    }

    async fn send_connection_packet(&self) -> Result<()> {
        let connection_packet = ConnectionPacket {
            meeting_id: self.options.meeting_id.clone(),
            ..Default::default()
        };

        let packet = PacketWrapper {
            packet_type: PacketType::CONNECTION.into(),
            email: self.options.userid.clone(),
            data: connection_packet.write_to_bytes()?,
            ..Default::default()
        };

        let data = packet.write_to_bytes()?;
        if let Some(tx) = &self.packet_tx {
            tx.send(data).await.map_err(|e| anyhow!("Send error: {e}"))?;
        }
        info!("Sent connection packet for '{}'", self.options.userid);
        Ok(())
    }

    fn start_heartbeat(&self, session: web_transport_quinn::Session) {
        let user_id = self.options.userid.clone();
        let quit = self.quit.clone();
        let video_enabled = self.video_enabled.clone();
        let audio_enabled = self.audio_enabled.clone();
        let screen_enabled = self.screen_enabled.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            // Skip the first immediate tick
            interval.tick().await;

            loop {
                interval.tick().await;
                if quit.load(Ordering::Relaxed) {
                    break;
                }

                let heartbeat = MediaPacket {
                    media_type: MediaType::HEARTBEAT.into(),
                    email: user_id.clone(),
                    timestamp: platform::now_ms(),
                    heartbeat_metadata: Some(HeartbeatMetadata {
                        video_enabled: video_enabled.load(Ordering::Relaxed),
                        audio_enabled: audio_enabled.load(Ordering::Relaxed),
                        screen_enabled: screen_enabled.load(Ordering::Relaxed),
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

                let data = packet.write_to_bytes().unwrap();
                if let Err(e) = Self::send_via_session(&session, data).await {
                    warn!("Failed to send heartbeat for {}: {}", user_id, e);
                } else {
                    debug!("Sent heartbeat for {}", user_id);
                }
            }
            info!("Heartbeat stopped for {}", user_id);
        });
    }

    async fn start_inbound_consumer(&self, session: web_transport_quinn::Session) {
        let user_id = self.options.userid.clone();
        let quit = self.quit.clone();
        // We can't move the callback into the task directly because
        // NativeClientOptions isn't Clone. Instead, use an Arc.
        // But we already have it as Box<dyn Fn + Send + Sync>, which we can wrap.
        // For now, just drain inbound streams. Full inbound handling is for later phases.
        let on_disconnected = Arc::new({
            // We need the userid for the error message
            let user_id = user_id.clone();
            move |e: String| {
                error!("Inbound consumer for {} ended: {}", user_id, e);
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
                        tokio::spawn(async move {
                            match stream.read_to_end(usize::MAX).await {
                                Ok(data) => {
                                    // Try to parse as PacketWrapper
                                    match PacketWrapper::parse_from_bytes(&data) {
                                        Ok(packet) => {
                                            debug!(
                                                "Received {:?} packet for {}",
                                                packet.packet_type.enum_value(),
                                                user_id
                                            );
                                            // Packet handling will be connected in Phase 3
                                        }
                                        Err(e) => {
                                            debug!(
                                                "Failed to parse inbound packet for {}: {}",
                                                user_id, e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!(
                                        "Error reading inbound stream for {}: {}",
                                        user_id, e
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        on_disconnected(format!("{e}"));
                        break;
                    }
                }
            }
            info!("Inbound consumer stopped for {}", user_id);
        });
    }
}
