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
mod ekg_renderer;
mod health_reporter;
mod inbound_stats;
mod token;
mod transport;
mod video_encoder;
mod video_producer;
mod websocket_client;
mod webtransport_client;

use audio_producer::AudioProducer;
use config::{BotConfig, ClientConfig, Manifest, Transport};
use ekg_renderer::EkgRenderer;
use health_reporter::{spawn_health_reporter, HealthReporterConfig};
use inbound_stats::InboundStats;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

    let (config, num_users) = BotConfig::from_args()?;
    info!(
        "Transport: {:?}, JWT auth: {}",
        config.transport,
        config.jwt_secret.is_some()
    );

    // Load conversation manifest
    let conv_dir = config.conversation_dir().to_string();
    let manifest_path = format!("{conv_dir}/manifest.yaml");
    let manifest = Manifest::from_file(&manifest_path)?;
    info!(
        "Manifest: {} participants, {} lines, {}ms pause",
        manifest.participants.len(),
        manifest.lines.len(),
        manifest.pause_ms
    );

    // Take first N participants (0 = all)
    let n = if num_users == 0 {
        manifest.participants.len()
    } else {
        num_users.min(manifest.participants.len())
    };
    let active_participants = &manifest.participants[..n];
    let active_names: HashSet<&str> = active_participants
        .iter()
        .map(|p| p.name.as_str())
        .collect();

    info!(
        "Active participants ({}): {}",
        n,
        active_participants
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Filter lines to active speakers only
    let active_lines: Vec<&config::Line> = manifest
        .lines
        .iter()
        .filter(|l| active_names.contains(l.speaker.as_str()))
        .collect();
    info!(
        "Active lines: {} of {} total",
        active_lines.len(),
        manifest.lines.len()
    );

    // Load per-line WAV audio
    info!("Loading audio clips...");
    let line_audio: Vec<Vec<f32>> = active_lines
        .iter()
        .map(|line| load_wav_samples(&format!("{conv_dir}/{}", line.audio_file)))
        .collect::<Result<_, _>>()?;

    // Stitch per-participant audio
    let pause_samples = (manifest.pause_ms as usize * 48000) / 1000;
    let mut participant_audio: HashMap<String, Vec<f32>> = HashMap::new();
    let mut total_samples: usize = 0;

    for p in active_participants {
        participant_audio.insert(p.name.clone(), Vec::new());
    }

    for (i, line) in active_lines.iter().enumerate() {
        let line_samples = line_audio[i].len();
        for p in active_participants {
            let audio = participant_audio.get_mut(&p.name).unwrap();
            if p.name == line.speaker {
                audio.extend_from_slice(&line_audio[i]);
            } else {
                audio.resize(audio.len() + line_samples, 0.0f32);
            }
            // Pause between lines
            audio.resize(audio.len() + pause_samples, 0.0f32);
        }
        total_samples = participant_audio.values().next().map_or(0, |a| a.len());
    }

    let loop_duration = Duration::from_millis((total_samples as u64 * 1000) / 48000);
    info!(
        "Stitched timeline: {:.1}s ({} samples), {} active lines for {} participants",
        loop_duration.as_secs_f64(),
        total_samples,
        active_lines.len(),
        n
    );

    // Spawn clients
    let shared_media_start = Instant::now();
    let ramp_up_delay = Duration::from_millis(config.ramp_up_delay_ms.unwrap_or(1000));
    let insecure = config.insecure.unwrap_or(false);

    if insecure {
        warn!("WARNING: Certificate verification disabled - connection is insecure!");
    }

    let mut client_handles = Vec::new();

    for (index, p) in active_participants.iter().enumerate() {
        let audio_data = participant_audio.remove(&p.name).unwrap();

        info!(
            "Starting client {} ({}) - audio: {} samples",
            index,
            p.name,
            audio_data.len(),
        );

        let bot_config = config.clone();
        let user_id = p.name.clone();
        let meeting_id = config.meeting_id.clone();
        let ekg_color = p.ekg_color;
        let media_start = shared_media_start;
        let ld = loop_duration;

        let handle = tokio::spawn(async move {
            if let Err(e) = run_client(
                bot_config,
                user_id,
                meeting_id,
                audio_data,
                ekg_color,
                insecure,
                media_start,
                ld,
            )
            .await
            {
                error!("Client failed: {}", e);
            }
        });

        client_handles.push(handle);

        if index < n - 1 {
            info!(
                "Waiting {}ms before starting next client",
                ramp_up_delay.as_millis()
            );
            time::sleep(ramp_up_delay).await;
        }
    }

    info!("All {} clients started, waiting for Ctrl+C", n);

    for handle in client_handles {
        let _ = handle.await;
    }

    info!("All clients finished");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_client(
    bot_config: BotConfig,
    user_id: String,
    meeting_id: String,
    audio_data: Vec<f32>,
    ekg_color: [u8; 3],
    insecure: bool,
    media_start: Instant,
    loop_duration: Duration,
) -> anyhow::Result<()> {
    info!("Initializing client: {}", user_id);

    let client_config = ClientConfig {
        user_id: user_id.clone(),
        meeting_id,
        enable_audio: true,
        enable_video: true,
    };

    let lobby_url = TransportClient::build_lobby_url(&bot_config, &client_config)?;
    // Redact JWT token from log output
    let display_url = lobby_url
        .as_str()
        .split("?token=")
        .next()
        .unwrap_or(lobby_url.as_str());
    info!(
        "Lobby URL: {}{}",
        display_url,
        if lobby_url.query().is_some() {
            "?token=<redacted>"
        } else {
            ""
        }
    );

    // Shared inbound stats — used by both the transport's inbound consumer
    // and the health reporter for per-sender packet rate tracking.
    let stats = Arc::new(Mutex::new(InboundStats::default()));

    let mut client = TransportClient::new(&bot_config.transport, client_config.clone());
    client.connect(&lobby_url, insecure, stats.clone()).await?;

    // Create packet channel for media + heartbeat + health producers
    let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task
    client.start_packet_sender(packet_rx).await;

    // For WebSocket transport, heartbeats go through the shared mpsc channel
    let quit = Arc::new(AtomicBool::new(false));
    if matches!(bot_config.transport, Transport::WebSocket) {
        spawn_heartbeat_producer(
            client_config.user_id.clone(),
            client_config.enable_audio,
            client_config.enable_video,
            packet_tx.clone(),
            quit.clone(),
        );
    }

    // Spawn health reporter — sends HealthPacket every 1s so senders can
    // observe this bot's received FPS and adjust their encoding tiers.
    let server_url_display = lobby_url
        .as_str()
        .split("?token=")
        .next()
        .unwrap_or(lobby_url.as_str())
        .to_string();
    spawn_health_reporter(
        HealthReporterConfig {
            client_config: client_config.clone(),
            transport: bot_config.transport.clone(),
            server_url: server_url_display,
        },
        stats,
        packet_tx.clone(),
        quit.clone(),
    );

    // Compute RMS for EKG video from stitched audio
    let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, 15);
    let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
    let renderer = EkgRenderer::new(ekg_color, 1280, 720);

    // Start media producers
    let audio_producer = AudioProducer::new(
        user_id.clone(),
        audio_data,
        packet_tx.clone(),
        media_start,
        loop_duration,
    )?;
    info!("Audio producer started for {}", user_id);

    let video_producer = VideoProducer::from_ekg(
        user_id.clone(),
        renderer,
        rms,
        max_rms,
        packet_tx.clone(),
        media_start,
        loop_duration,
    )?;
    info!("Video producer started for {}", user_id);

    info!("Client {} running", user_id);

    // Keep the client running
    tokio::signal::ctrl_c().await?;

    info!("Shutting down client: {}", user_id);

    quit.store(true, Ordering::Relaxed);
    client.stop().await;
    drop(audio_producer);
    drop(video_producer);

    info!("Client {} shut down cleanly", user_id);
    Ok(())
}

/// Load WAV file samples as normalized f32 PCM.
fn load_wav_samples(path: &str) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| anyhow::anyhow!("Failed to open WAV file {}: {}", path, e))?;
    let spec = reader.spec();

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| Ok(s? as f32 / 32768.0))
            .collect::<Result<_, hound::Error>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    };

    Ok(samples)
}
