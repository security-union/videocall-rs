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
mod inbound_stats;
mod token;
mod transport;
mod video_encoder;
mod video_producer;
mod websocket_client;
mod webtransport_client;

use audio_producer::AudioProducer;
use config::{BotConfig, ClientConfig, Transport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{error, info, warn};
use transport::TransportClient;
use video_producer::VideoProducer;
use websocket_client::spawn_heartbeat_producer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("Starting videocall synthetic client bot");

    let config = BotConfig::from_args()?;
    info!("Loaded configuration for {} clients", config.clients.len());
    info!(
        "Transport: {:?}, JWT auth: {}",
        config.transport,
        config.jwt_secret.is_some()
    );

    let ramp_up_delay = Duration::from_millis(config.ramp_up_delay_ms.unwrap_or(1000));
    let insecure = config.insecure.unwrap_or(false);

    if insecure {
        warn!("WARNING: Certificate verification disabled - connection is insecure!");
    }

    let mut client_handles = Vec::new();
    let total_clients = config.clients.len();

    for (index, client_config) in config.clients.iter().enumerate() {
        info!(
            "Starting client {} ({}) - audio: {}, video: {}",
            index, client_config.user_id, client_config.enable_audio, client_config.enable_video
        );

        let bot_config = config.clone();
        let client_config = client_config.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = run_client(bot_config, client_config, insecure).await {
                error!("Client failed: {}", e);
            }
        });

        client_handles.push(handle);

        if index < total_clients - 1 {
            info!(
                "Waiting {}ms before starting next client",
                ramp_up_delay.as_millis()
            );
            time::sleep(ramp_up_delay).await;
        }
    }

    info!("All clients started, waiting for completion");

    for handle in client_handles {
        let _ = handle.await;
    }

    info!("All clients finished");
    Ok(())
}

async fn run_client(
    bot_config: BotConfig,
    config: ClientConfig,
    insecure: bool,
) -> anyhow::Result<()> {
    info!("Initializing client: {}", config.user_id);

    let lobby_url = TransportClient::build_lobby_url(&bot_config, &config)?;
    info!("Lobby URL: {}", lobby_url);

    let mut client = TransportClient::new(&bot_config.transport, config.clone());
    client.connect(&lobby_url, insecure).await?;

    // Create packet channel for media + heartbeat producers
    let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task
    client.start_packet_sender(packet_rx).await;

    // For WebSocket transport, heartbeats go through the shared mpsc channel
    // (WebTransport client handles heartbeats internally via its own session)
    let quit = Arc::new(AtomicBool::new(false));
    if matches!(bot_config.transport, Transport::WebSocket) {
        spawn_heartbeat_producer(
            config.user_id.clone(),
            config.enable_audio,
            config.enable_video,
            packet_tx.clone(),
            quit.clone(),
        );
    }

    // Compute loop duration from WAV file so audio and video wrap at the same point.
    let wav_path = config.audio_file.as_deref().unwrap_or("BundyBests2.wav");
    let loop_duration = AudioProducer::wav_duration(wav_path).unwrap_or(Duration::from_secs(60));
    info!("Media loop duration: {:.3}s", loop_duration.as_secs_f64());

    // Shared media clock — both audio and video derive position from this epoch,
    // ensuring they stay in sync even if one producer starts slightly later.
    let media_start = std::time::Instant::now();

    // Start media producers based on configuration
    let mut audio_producer: Option<AudioProducer> = None;
    let mut video_producer: Option<VideoProducer> = None;

    if config.enable_audio {
        info!("Starting audio producer for {}", config.user_id);
        match AudioProducer::from_wav_file(
            config.user_id.clone(),
            wav_path,
            packet_tx.clone(),
            media_start,
            loop_duration,
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
        let img_dir = config.image_dir.as_deref().unwrap_or(".");
        match VideoProducer::from_image_sequence(
            config.user_id.clone(),
            img_dir,
            packet_tx.clone(),
            media_start,
            loop_duration,
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
    quit.store(true, Ordering::Relaxed);
    client.stop();
    if let Some(mut audio) = audio_producer {
        audio.stop();
    }
    if let Some(mut video) = video_producer {
        video.stop();
    }

    info!("Client {} shut down cleanly", config.user_id);
    Ok(())
}
