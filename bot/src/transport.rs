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

//! Transport-agnostic client wrapper.
//!
//! Selects WebSocket or WebTransport based on config, builds the lobby URL
//! (with JWT when configured), and delegates to the concrete client.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Receiver;
use tracing::info;
use url::Url;

use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;
use crate::token;
use crate::websocket_client::WebSocketClient;
use crate::webtransport_client::WebTransportClient;

/// Hook installed by the netsim shim. When present, inbound readers hand
/// each raw payload to the hook instead of calling `InboundStats::record_packet`
/// directly — the hook typically posts the payload to a `NetSimInbound` task
/// that applies the downlink profile and then records it after the delay.
///
/// Left `None` for passthrough bots so the hot path is a direct method call.
pub type InboundHook = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// Coarse media-type label for an outbound [`OutboundFrame`].
///
/// Producers already know the packet type at construction time — tagging the
/// frame here lets the outbound shim + metrics-counting tasks pick a
/// Prometheus label without re-parsing the protobuf on the hot path. The
/// variant set is intentionally small and stable so the `media_type` label
/// cardinality on `bot_packets_sent_total` stays bounded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MediaTypeLabel {
    Audio,
    Video,
    Health,
    Heartbeat,
    Diagnostics,
    Other,
}

impl MediaTypeLabel {
    /// Stable string label used as the `media_type` Prometheus label value.
    /// Kept here (not in metrics_server) so non-metrics builds still get the
    /// same strings for debug logs.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaTypeLabel::Audio => "audio",
            MediaTypeLabel::Video => "video",
            MediaTypeLabel::Health => "health",
            MediaTypeLabel::Heartbeat => "heartbeat",
            MediaTypeLabel::Diagnostics => "diagnostics",
            MediaTypeLabel::Other => "other",
        }
    }
}

/// A payload produced by an audio/video/health/heartbeat producer, tagged
/// with its coarse media type so downstream consumers (outbound shim,
/// metrics-counting shim) can label Prometheus counters without re-parsing
/// the serialized protobuf.
///
/// The `bytes` field is the already-serialized `PacketWrapper` — consumers
/// forward it verbatim to the transport sender.
#[derive(Debug)]
pub struct OutboundFrame {
    pub kind: MediaTypeLabel,
    pub bytes: Vec<u8>,
}

impl OutboundFrame {
    pub fn new(kind: MediaTypeLabel, bytes: Vec<u8>) -> Self {
        Self { kind, bytes }
    }
}

#[allow(clippy::large_enum_variant)]
pub enum TransportClient {
    WebSocket(WebSocketClient),
    WebTransport(WebTransportClient),
}

impl TransportClient {
    pub fn new(
        transport: &Transport,
        config: ClientConfig,
        #[cfg(feature = "metrics")] metrics: Option<std::sync::Arc<BotMetrics>>,
    ) -> Self {
        match transport {
            Transport::WebSocket => TransportClient::WebSocket(WebSocketClient::new(
                config,
                #[cfg(feature = "metrics")]
                metrics,
            )),
            Transport::WebTransport => {
                TransportClient::WebTransport(WebTransportClient::new(config))
            }
        }
    }

    /// Build the lobby URL for this client, minting a JWT if configured.
    pub fn build_lobby_url(
        transport: &Transport,
        server_url: &Url,
        jwt_secret: Option<&str>,
        user_id: &str,
        meeting_id: &str,
        token_ttl_secs: u64,
    ) -> anyhow::Result<Url> {
        let base = server_url.to_string();
        let base = base.trim_end_matches('/');

        let url_string = if let Some(secret) = jwt_secret {
            let token = token::mint_token(secret, user_id, meeting_id, token_ttl_secs)?;
            format!("{base}/lobby?token={token}")
        } else {
            format!("{base}/lobby/{user_id}/{meeting_id}")
        };

        // For WebSocket, convert https:// to wss:// and http:// to ws://
        let url_string = match transport {
            Transport::WebSocket => url_string
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1),
            Transport::WebTransport => url_string,
        };

        Url::parse(&url_string).map_err(|e| anyhow::anyhow!("Invalid lobby URL: {e}"))
    }

    pub async fn connect(
        &mut self,
        lobby_url: &Url,
        insecure: bool,
        stats: Arc<Mutex<InboundStats>>,
        is_speaking: Arc<AtomicBool>,
        inbound_hook: Option<InboundHook>,
    ) -> anyhow::Result<()> {
        match self {
            TransportClient::WebSocket(c) => {
                if insecure {
                    info!("Note: --insecure flag has no effect on WebSocket (TLS handled by tokio-tungstenite with system roots)");
                }
                c.connect(lobby_url, stats, inbound_hook).await
            }
            TransportClient::WebTransport(c) => {
                c.connect(lobby_url, insecure, stats, is_speaking, inbound_hook)
                    .await
            }
        }
    }

    pub async fn start_packet_sender(&mut self, packet_receiver: Receiver<Vec<u8>>) {
        match self {
            TransportClient::WebSocket(c) => c.start_packet_sender(packet_receiver).await,
            TransportClient::WebTransport(c) => c.start_packet_sender(packet_receiver).await,
        }
    }

    pub async fn stop(&mut self) {
        match self {
            TransportClient::WebSocket(c) => c.stop().await,
            TransportClient::WebTransport(c) => c.stop(),
        }
    }
}
