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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{error::TrySendError, Receiver, Sender};
use tracing::info;
use url::Url;
use videocall_meeting_types::mint::{self, LobbyAuth};

use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;
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

/// Billing bucket behind the `_audio` / `_video` / `_control` suffixes on the
/// `ws_offered_bytes_*` fields. No `Screen` arm: the bot never publishes one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum WebSocketStreamBucket {
    Audio,
    Video,
    Control,
}

impl MediaTypeLabel {
    /// Lets a test assert over the whole set. Keep in step with the exhaustive
    /// matches below, which are what fail to compile when a variant is added.
    #[cfg(test)]
    pub(crate) const ALL: [MediaTypeLabel; 6] = [
        MediaTypeLabel::Audio,
        MediaTypeLabel::Video,
        MediaTypeLabel::Health,
        MediaTypeLabel::Heartbeat,
        MediaTypeLabel::Diagnostics,
        MediaTypeLabel::Other,
    ];

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

    /// Which `ws_*_bytes_*` bucket this label's bytes are billed to.
    pub(crate) fn websocket_stream_bucket(self) -> WebSocketStreamBucket {
        match self {
            MediaTypeLabel::Audio => WebSocketStreamBucket::Audio,
            MediaTypeLabel::Video => WebSocketStreamBucket::Video,
            MediaTypeLabel::Health
            | MediaTypeLabel::Heartbeat
            | MediaTypeLabel::Diagnostics
            | MediaTypeLabel::Other => WebSocketStreamBucket::Control,
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

/// Cumulative un-framed bytes offered to the WebSocket send path, per bucket.
/// Offered only — `set_ws_stream_bytes` says why the bot bills no drops.
#[derive(Default)]
pub struct WebSocketStreamByteCounters {
    offered_audio: AtomicU64,
    offered_video: AtomicU64,
    offered_control: AtomicU64,
}

/// Point-in-time read of [`WebSocketStreamByteCounters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WebSocketStreamByteSnapshot {
    pub offered_audio: u64,
    pub offered_video: u64,
    pub offered_control: u64,
}

impl WebSocketStreamByteCounters {
    pub(crate) fn record_offered(&self, kind: MediaTypeLabel, bytes: u64) {
        match kind.websocket_stream_bucket() {
            WebSocketStreamBucket::Audio => {
                self.offered_audio.fetch_add(bytes, Ordering::Relaxed);
            }
            WebSocketStreamBucket::Video => {
                self.offered_video.fetch_add(bytes, Ordering::Relaxed);
            }
            WebSocketStreamBucket::Control => {
                self.offered_control.fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> WebSocketStreamByteSnapshot {
        WebSocketStreamByteSnapshot {
            offered_audio: self.offered_audio.load(Ordering::Relaxed),
            offered_video: self.offered_video.load(Ordering::Relaxed),
            offered_control: self.offered_control.load(Ordering::Relaxed),
        }
    }
}

/// Wraps the outbound `Sender` so every producer bills at one chokepoint.
#[derive(Clone)]
pub struct OutboundFrameSender {
    tx: Sender<OutboundFrame>,
    websocket_stream_bytes: Option<Arc<WebSocketStreamByteCounters>>,
}

impl OutboundFrameSender {
    pub fn new(tx: Sender<OutboundFrame>) -> Self {
        Self {
            tx,
            websocket_stream_bytes: None,
        }
    }

    pub fn with_websocket_accounting(
        tx: Sender<OutboundFrame>,
        websocket_stream_bytes: Arc<WebSocketStreamByteCounters>,
    ) -> Self {
        Self {
            tx,
            websocket_stream_bytes: Some(websocket_stream_bytes),
        }
    }

    /// Bills `offered` before the send is attempted, so a rejected frame still
    /// counts as demand.
    pub fn try_send(&self, frame: OutboundFrame) -> Result<(), TrySendError<OutboundFrame>> {
        if let Some(counters) = &self.websocket_stream_bytes {
            counters.record_offered(frame.kind, frame.bytes.len() as u64);
        }
        self.tx.try_send(frame)
    }
}

impl From<Sender<OutboundFrame>> for OutboundFrameSender {
    fn from(tx: Sender<OutboundFrame>) -> Self {
        Self::new(tx)
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

    /// Build the lobby URL for this client, minting a fresh JWT per call.
    pub fn build_lobby_url(
        transport: &Transport,
        server_url: &Url,
        auth: &LobbyAuth,
        user_id: &str,
        meeting_id: &str,
    ) -> anyhow::Result<Url> {
        let url_string = mint::build_lobby_url(server_url.as_str(), auth, user_id, meeting_id)
            .map_err(|e| anyhow::anyhow!("failed to build lobby URL: {e}"))?;

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

#[cfg(test)]
mod tests {
    use super::{
        MediaTypeLabel, OutboundFrame, OutboundFrameSender, TransportClient, WebSocketStreamBucket,
        WebSocketStreamByteCounters,
    };
    use crate::config::{BotConfig, Transport};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use url::Url;
    use videocall_meeting_types::mint::LobbyAuth;

    fn secret_auth() -> LobbyAuth {
        LobbyAuth::Secret {
            secret: "secret".into(),
            ttl_secs: 60,
        }
    }

    #[test]
    fn websocket_stream_bucket_mapping_covers_every_media_label() {
        assert_eq!(
            MediaTypeLabel::Audio.websocket_stream_bucket(),
            WebSocketStreamBucket::Audio
        );
        assert_eq!(
            MediaTypeLabel::Video.websocket_stream_bucket(),
            WebSocketStreamBucket::Video
        );
        for kind in MediaTypeLabel::ALL {
            if matches!(kind, MediaTypeLabel::Audio | MediaTypeLabel::Video) {
                continue;
            }
            assert_eq!(
                kind.websocket_stream_bucket(),
                WebSocketStreamBucket::Control,
                "{kind:?} must bill to the control bucket"
            );
        }
    }

    #[test]
    fn websocket_offered_bytes_are_billed_even_when_the_channel_rejects() {
        let (tx, _rx) = mpsc::channel::<OutboundFrame>(1);
        let counters = Arc::new(WebSocketStreamByteCounters::default());
        let sender = OutboundFrameSender::with_websocket_accounting(tx, counters.clone());

        sender
            .try_send(OutboundFrame::new(MediaTypeLabel::Audio, vec![0; 3]))
            .expect("first frame fits");
        assert!(
            sender
                .try_send(OutboundFrame::new(MediaTypeLabel::Video, vec![0; 5]))
                .is_err(),
            "second frame must hit the full bounded channel"
        );

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.offered_audio, 3);
        assert_eq!(snapshot.offered_video, 5);
        assert_eq!(snapshot.offered_control, 0);
    }

    #[test]
    fn build_lobby_url_uses_ws_scheme_and_trims_trailing_slash() {
        let server_url = Url::parse("https://relay.example.com/").unwrap();
        let url = TransportClient::build_lobby_url(
            &Transport::WebSocket,
            &server_url,
            &LobbyAuth::DeprecatedPath,
            "bot-1",
            "meeting-1",
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "wss://relay.example.com/lobby/bot-1/meeting-1"
        );
    }

    #[test]
    fn build_lobby_url_preserves_webtransport_scheme_and_port() {
        let server_url = Url::parse("https://relay.example.com:4443/base").unwrap();
        let url = TransportClient::build_lobby_url(
            &Transport::WebTransport,
            &server_url,
            &LobbyAuth::DeprecatedPath,
            "bot-1",
            "meeting-1",
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "https://relay.example.com:4443/base/lobby/bot-1/meeting-1"
        );
    }

    #[test]
    fn build_lobby_url_mints_token_for_authenticated_path() {
        let server_url = Url::parse("https://relay.example.com").unwrap();
        let url = TransportClient::build_lobby_url(
            &Transport::WebSocket,
            &server_url,
            &secret_auth(),
            "bot-1",
            "meeting-1",
        )
        .unwrap();

        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.path(), "/lobby");
        assert!(url.query().unwrap_or_default().starts_with("token="));
    }

    #[test]
    fn build_lobby_url_encodes_special_characters_in_path() {
        let server_url = Url::parse("https://relay.example.com/").unwrap();

        let url = TransportClient::build_lobby_url(
            &Transport::WebSocket,
            &server_url,
            &LobbyAuth::DeprecatedPath,
            "user/admin",
            "room?id=1#top",
        )
        .unwrap();

        assert_eq!(url.scheme(), "wss");
        assert!(
            !url.path().contains('?'),
            "path must not contain raw '?' — got: {}",
            url.path()
        );
        assert!(
            !url.path().contains('#'),
            "path must not contain raw '#' — got: {}",
            url.path()
        );
        let segments: Vec<_> = url.path().split('/').collect();
        assert_eq!(segments.len(), 4, "expected /lobby/<user>/<meeting>");
        assert_eq!(segments[1], "lobby");

        let url = TransportClient::build_lobby_url(
            &Transport::WebTransport,
            &server_url,
            &LobbyAuth::DeprecatedPath,
            "bot-1",
            "salle-réunion",
        )
        .unwrap();
        assert!(
            url.as_str().contains("salle-r%C3%A9union"),
            "unicode should be percent-encoded — got: {}",
            url.as_str()
        );
    }

    /// A config with neither a secret nor an explicit opt-in must NOT connect
    /// unauthenticated (issue #2298).
    #[test]
    fn a_config_without_a_secret_refuses_to_join_unauthenticated() {
        let config = BotConfig::default();
        let err = config
            .resolve_lobby_auth()
            .expect_err("a credential-less config must not resolve to a joinable auth mode");
        assert!(
            err.to_string().contains("no room access token"),
            "expected a missing-credential error — got: {}",
            err
        );
    }

    #[test]
    fn a_config_with_a_secret_defaults_to_token_auth() {
        // Even with the deprecated path explicitly allowed, the secret wins.
        let config = BotConfig {
            jwt_secret: Some("secret".into()),
            allow_deprecated_path: Some(true),
            ..Default::default()
        };

        let url = TransportClient::build_lobby_url(
            &Transport::WebTransport,
            &Url::parse("https://relay.example.com").unwrap(),
            &config.resolve_lobby_auth().unwrap(),
            "bot-1",
            "meeting-1",
        )
        .unwrap();

        assert_eq!(url.path(), "/lobby");
        assert!(url.query().unwrap_or_default().starts_with("token="));
    }

    #[test]
    fn a_config_that_opts_in_still_reaches_the_deprecated_path() {
        let config = BotConfig {
            allow_deprecated_path: Some(true),
            ..Default::default()
        };

        let url = TransportClient::build_lobby_url(
            &Transport::WebTransport,
            &Url::parse("https://relay.example.com").unwrap(),
            &config.resolve_lobby_auth().unwrap(),
            "bot-1",
            "meeting-1",
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "https://relay.example.com/lobby/bot-1/meeting-1"
        );
    }
}
