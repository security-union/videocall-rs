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

use tokio::sync::mpsc::Receiver;
use tracing::info;
use url::Url;

use crate::config::{BotConfig, ClientConfig, Transport};
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
        bot_config: &BotConfig,
        client_config: &ClientConfig,
    ) -> anyhow::Result<Url> {
        let base = bot_config.server_url()?.to_string();
        let base = base.trim_end_matches('/');

        let url_string = if let Some(ref secret) = bot_config.jwt_secret {
            let token = token::mint_token(
                secret,
                &client_config.user_id,
                &client_config.meeting_id,
                bot_config.token_ttl_secs(),
            )?;
            format!("{base}/lobby?token={token}")
        } else {
            format!(
                "{base}/lobby/{}/{}",
                client_config.user_id, client_config.meeting_id
            )
        };

        // For WebSocket, convert https:// to wss:// and http:// to ws://
        let url_string = match bot_config.transport {
            Transport::WebSocket => url_string
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1),
            Transport::WebTransport => url_string,
        };

        Url::parse(&url_string).map_err(|e| anyhow::anyhow!("Invalid lobby URL: {e}"))
    }

    pub async fn connect(&mut self, lobby_url: &Url, insecure: bool) -> anyhow::Result<()> {
        match self {
            TransportClient::WebSocket(c) => {
                if insecure {
                    info!("Note: --insecure flag has no effect on WebSocket (TLS handled by tokio-tungstenite with system roots)");
                }
                c.connect(lobby_url).await
            }
            TransportClient::WebTransport(c) => c.connect(lobby_url, insecure).await,
        }
    }

    pub async fn start_packet_sender(&mut self, packet_receiver: Receiver<Vec<u8>>) {
        match self {
            TransportClient::WebSocket(c) => c.start_packet_sender(packet_receiver).await,
            TransportClient::WebTransport(c) => c.start_packet_sender(packet_receiver).await,
        }
    }

    pub fn stop(&self) {
        match self {
            TransportClient::WebSocket(c) => c.stop(),
            TransportClient::WebTransport(c) => c.stop(),
        }
    }
}
