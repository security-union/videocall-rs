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

mod audio_producer;
mod config;
mod video_encoder;
mod video_producer;

use audio_producer::AudioProducer;
use config::{BotConfig, ClientConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};
use video_producer::VideoProducer;
use videocall_client::{NativeClientOptions, NativeVideoCallClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("Starting videocall synthetic client bot");

    // Load configuration
    let config = BotConfig::from_env_or_default()?;
    info!("Loaded configuration for {} clients", config.clients.len());

    let server_url = config.server_url()?;
    let ramp_up_delay = Duration::from_millis(config.ramp_up_delay_ms.unwrap_or(1000));
    let insecure = config.insecure.unwrap_or(false);

    if insecure {
        warn!("WARNING: Certificate verification disabled - connection is insecure!");
    }

    // Start clients with linear ramp-up
    let mut client_handles = Vec::new();
    let total_clients = config.clients.len();

    for (index, client_config) in config.clients.into_iter().enumerate() {
        info!(
            "Starting client {} ({}) - audio: {}, video: {}",
            index, client_config.user_id, client_config.enable_audio, client_config.enable_video
        );

        let server_url_clone = server_url.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = run_client(client_config, server_url_clone, insecure).await {
                error!("Client failed: {}", e);
            }
        });

        client_handles.push(handle);

        // Linear ramp-up delay between client starts
        if index < total_clients - 1 {
            info!(
                "Waiting {}ms before starting next client",
                ramp_up_delay.as_millis()
            );
            time::sleep(ramp_up_delay).await;
        }
    }

    info!("All clients started, waiting for completion");

    // Wait for all clients to complete
    for handle in client_handles {
        let _ = handle.await;
    }

    info!("All clients finished");
    Ok(())
}

async fn run_client(
    config: ClientConfig,
    server_url: url::Url,
    insecure: bool,
) -> anyhow::Result<()> {
    info!("Initializing client: {}", config.user_id);

    // Build the WebTransport URL
    let webtransport_url = format!(
        "{}/lobby/{}/{}",
        server_url.as_str().trim_end_matches('/'),
        config.user_id,
        config.meeting_id
    );

    // Create NativeVideoCallClient from videocall-client
    let user_id = config.user_id.clone();
    let mut client = NativeVideoCallClient::new(NativeClientOptions {
        userid: config.user_id.clone(),
        meeting_id: config.meeting_id.clone(),
        webtransport_url,
        insecure,
        on_inbound_packet: Box::new(|_pkt| {
            // Bot doesn't consume inbound packets for now
        }),
        on_connected: Box::new({
            let user_id = user_id.clone();
            move || info!("Client {user_id} connected")
        }),
        on_disconnected: Box::new({
            let user_id = user_id.clone();
            move |err| warn!("Client {user_id} disconnected: {err}")
        }),
        enable_e2ee: false,
    });

    client.connect().await?;

    // Wrap client in Arc for sharing with producers
    let client = Arc::new(client);

    // Set media enabled flags
    if config.enable_video {
        client.set_video_enabled(true);
    }
    if config.enable_audio {
        client.set_audio_enabled(true);
    }

    // Start media producers based on configuration
    let mut audio_producer: Option<AudioProducer> = None;
    let mut video_producer: Option<VideoProducer> = None;

    if config.enable_audio {
        info!("Starting audio producer for {}", config.user_id);
        match AudioProducer::from_wav_file(
            config.user_id.clone(),
            "BundyBests2.wav",
            client.clone(),
        ) {
            Ok(producer) => {
                audio_producer = Some(producer);
                info!("Audio producer started for {}", config.user_id);
            }
            Err(e) => {
                warn!(
                    "Failed to start audio producer for {}: {}",
                    config.user_id, e
                );
            }
        }
    }

    if config.enable_video {
        info!("Starting video producer for {}", config.user_id);
        match VideoProducer::from_image_sequence(
            config.user_id.clone(),
            ".", // Images are in current directory (bot working dir)
            client.clone(),
        ) {
            Ok(producer) => {
                video_producer = Some(producer);
                info!("Video producer started for {}", config.user_id);
            }
            Err(e) => {
                warn!(
                    "Failed to start video producer for {}: {}",
                    config.user_id, e
                );
            }
        }
    }

    info!(
        "Client {} running with audio: {}, video: {}",
        config.user_id,
        audio_producer.is_some(),
        video_producer.is_some()
    );

    // Keep the client running
    tokio::signal::ctrl_c().await?;

    info!("Shutting down client: {}", config.user_id);

    // Clean shutdown
    if let Some(mut audio) = audio_producer {
        audio.stop();
    }
    if let Some(mut video) = video_producer {
        video.stop();
    }

    info!("Client {} shut down cleanly", config.user_id);
    Ok(())
}
