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
use crate::token;
use crate::websocket_client::WebSocketClient;
use crate::webtransport_client::WebTransportClient;

pub enum TransportClient {
    WebSocket(WebSocketClient),
    WebTransport(WebTransportClient),
}

impl TransportClient {
    pub fn new(transport: &Transport, config: ClientConfig) -> Self {
        match transport {
            Transport::WebSocket => TransportClient::WebSocket(WebSocketClient::new(config)),
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
    ) -> anyhow::Result<()> {
        match self {
            TransportClient::WebSocket(c) => {
                if insecure {
                    info!("Note: --insecure flag has no effect on WebSocket (TLS handled by tokio-tungstenite with system roots)");
                }
                c.connect(lobby_url, stats).await
            }
            TransportClient::WebTransport(c) => {
                c.connect(lobby_url, insecure, stats, is_speaking).await
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
