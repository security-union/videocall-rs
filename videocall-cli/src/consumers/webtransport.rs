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

use super::camera_synk::CameraSynk;
use crate::cli_args::Stream;
use std::sync::Arc;
use tracing::info;
use videocall_client::{NativeClientOptions, NativeVideoCallClient};

/// WebTransport client backed by `NativeVideoCallClient` from `videocall-client`.
///
/// This replaces the previous custom WebTransport implementation, reusing the
/// shared connection lifecycle (heartbeat, connection packet, etc.) from the
/// videocall-client crate.
pub struct WebTransportClient {
    options: Stream,
    client: Option<Arc<NativeVideoCallClient>>,
}

impl WebTransportClient {
    pub fn new(options: Stream) -> Self {
        Self {
            options,
            client: None,
        }
    }
}

impl CameraSynk for WebTransportClient {
    async fn connect(&mut self) -> anyhow::Result<()> {
        let mut url = self.options.url.clone();
        url.set_path(&format!(
            "/lobby/{}/{}",
            self.options.user_id, self.options.meeting_id
        ));

        let webtransport_url = url.to_string();
        let user_id = self.options.user_id.clone();
        let insecure = self.options.insecure_skip_verify;

        info!("Connecting to {} as {}", webtransport_url, user_id);

        let mut native_client = NativeVideoCallClient::new(NativeClientOptions {
            userid: user_id.clone(),
            meeting_id: self.options.meeting_id.clone(),
            webtransport_url,
            insecure,
            on_inbound_packet: Box::new(|_pkt| {
                // CLI doesn't consume inbound packets
            }),
            on_connected: Box::new({
                let user_id = user_id.clone();
                move || info!("Client {user_id} connected")
            }),
            on_disconnected: Box::new({
                let user_id = user_id.clone();
                move |err| tracing::warn!("Client {user_id} disconnected: {err}")
            }),
            enable_e2ee: false,
        });

        native_client.connect().await?;
        native_client.set_video_enabled(true);

        self.client = Some(Arc::new(native_client));
        Ok(())
    }

    async fn send_packet(&self, data: Vec<u8>) -> anyhow::Result<()> {
        if let Some(client) = &self.client {
            client.send_raw(data)
        } else {
            Err(anyhow::anyhow!("Not connected"))
        }
    }
}
