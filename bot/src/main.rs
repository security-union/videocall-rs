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
mod costume_renderer;
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
use config::{BotConfig, ClientConfig, Manifest, Transport, VideoMode};
use costume_renderer::CostumeRenderer;
use ekg_renderer::EkgRenderer;
use health_reporter::{spawn_health_reporter, HealthReporterConfig};
use inbound_stats::InboundStats;
use std::collections::{HashMap, HashSet};
use std::path::Path;
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
        "Config: ws_url={:?}, wt_url={:?}, wt_ratio={:?}, video_mode={:?}, \
         warmup={}s, broadcasters={}, JWT auth={}",
        config.ws_url,
        config.wt_url,
        config.wt_ratio,
        config.video_mode,
        config.warmup_secs(),
        config.broadcasters(),
        config.jwt_secret.is_some(),
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

    // Determine broadcaster/observer split
    let broadcaster_count = config.broadcasters();
    let broadcaster_names: HashSet<&str> = if broadcaster_count == 0 {
        // 0 means all broadcast
        active_names.clone()
    } else {
        active_participants
            .iter()
            .take(broadcaster_count)
            .map(|p| p.name.as_str())
            .collect()
    };

    info!(
        "Active participants ({}): {} | Broadcasters ({}): {}",
        n,
        active_participants
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        broadcaster_names.len(),
        broadcaster_names
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
    );

    // Filter lines to broadcaster speakers only (observers have no audio lines)
    let active_lines: Vec<&config::Line> = manifest
        .lines
        .iter()
        .filter(|l| broadcaster_names.contains(l.speaker.as_str()))
        .collect();
    info!(
        "Active lines: {} of {} total (broadcaster speakers only)",
        active_lines.len(),
        manifest.lines.len()
    );

    // Load per-line WAV audio
    info!("Loading audio clips...");
    let line_audio: Vec<Vec<f32>> = active_lines
        .iter()
        .map(|line| load_wav_samples(&format!("{conv_dir}/{}", line.audio_file)))
        .collect::<Result<_, _>>()?;

    // Stitch per-participant audio (plain conversation audio, no warmup padding)
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

    // Media start via OnceCell -- set AFTER all bots are spawned + warmup sleep
    let media_start_cell: Arc<tokio::sync::OnceCell<Instant>> =
        Arc::new(tokio::sync::OnceCell::new());

    // Spawn clients
    let ramp_up_delay = Duration::from_millis(config.ramp_up_delay_ms.unwrap_or(1000));
    let insecure = config.insecure.unwrap_or(false);

    if insecure {
        warn!("WARNING: Certificate verification disabled - connection is insecure!");
    }

    let mut client_handles = Vec::new();

    for (index, p) in active_participants.iter().enumerate() {
        let audio_data = participant_audio.remove(&p.name).unwrap();
        let is_broadcaster = broadcaster_names.contains(p.name.as_str());

        info!(
            "Starting client {} ({}) - audio: {} samples, broadcaster: {}",
            index,
            p.name,
            audio_data.len(),
            is_broadcaster,
        );

        let bot_config = config.clone();
        let user_id = p.name.clone();
        let meeting_id = config.meeting_id.clone();
        let ekg_color = p.ekg_color;
        let costume_dir = p.costume_dir.clone();
        let cell = media_start_cell.clone();
        let ld = loop_duration;
        let total_bots = n;

        let handle = tokio::spawn(async move {
            if let Err(e) = run_client(
                bot_config,
                user_id,
                meeting_id,
                audio_data,
                ekg_color,
                costume_dir,
                insecure,
                cell,
                ld,
                index,
                total_bots,
                is_broadcaster,
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

    // All bots spawned -- wait warmup then start media
    let warmup = config.warmup_secs();
    info!(
        "All {} clients spawned, waiting {}s warmup before starting media",
        n, warmup
    );
    time::sleep(Duration::from_secs(warmup)).await;

    let now = Instant::now();
    let _ = media_start_cell.set(now);
    info!("Media start signal sent at {:?}", now);

    info!("All {} clients running, waiting for Ctrl+C", n);

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
    costume_dir: Option<String>,
    insecure: bool,
    media_start_cell: Arc<tokio::sync::OnceCell<Instant>>,
    loop_duration: Duration,
    bot_index: usize,
    total_bots: usize,
    is_broadcaster: bool,
) -> anyhow::Result<()> {
    info!(
        "Initializing client: {} (broadcaster={})",
        user_id, is_broadcaster
    );

    // Resolve transport for this bot
    let (resolved_transport, server_url) = bot_config.resolve_transport(bot_index, total_bots)?;

    let client_config = ClientConfig {
        user_id: user_id.clone(),
        meeting_id,
        enable_audio: is_broadcaster,
        enable_video: is_broadcaster,
    };

    let lobby_url = TransportClient::build_lobby_url(
        &resolved_transport,
        &server_url,
        bot_config.jwt_secret.as_deref(),
        &client_config.user_id,
        &client_config.meeting_id,
        bot_config.token_ttl_secs(),
    )?;
    // Redact JWT token from log output
    let display_url = lobby_url
        .as_str()
        .split("?token=")
        .next()
        .unwrap_or(lobby_url.as_str());
    info!(
        "[{}] Transport: {:?}, Lobby URL: {}{}",
        user_id,
        resolved_transport,
        display_url,
        if lobby_url.query().is_some() {
            "?token=<redacted>"
        } else {
            ""
        }
    );

    // Shared inbound stats -- used by both the transport's inbound consumer
    // and the health reporter for per-sender packet rate tracking.
    let stats = Arc::new(Mutex::new(InboundStats::default()));

    // Shared is_speaking flag -- audio producer sets, heartbeat/video reads
    let is_speaking = Arc::new(AtomicBool::new(false));

    let mut client = TransportClient::new(&resolved_transport, client_config.clone());
    client
        .connect(&lobby_url, insecure, stats.clone(), is_speaking.clone())
        .await?;

    // Create packet channel for media + heartbeat + health producers
    let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task
    client.start_packet_sender(packet_rx).await;

    // For WebSocket transport, heartbeats go through the shared mpsc channel
    let quit = Arc::new(AtomicBool::new(false));
    if matches!(resolved_transport, Transport::WebSocket) {
        spawn_heartbeat_producer(
            client_config.user_id.clone(),
            client_config.enable_audio,
            client_config.enable_video,
            packet_tx.clone(),
            quit.clone(),
            is_speaking.clone(),
        );
    }

    // Spawn health reporter -- sends HealthPacket every 1s so senders can
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
            transport: resolved_transport.clone(),
            server_url: server_url_display,
        },
        stats,
        packet_tx.clone(),
        quit.clone(),
    );

    // Wait for media start signal from main
    let media_start = loop {
        if let Some(t) = media_start_cell.get() {
            break *t;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    info!("[{}] Media start received, beginning producers", user_id);

    // Only spawn media producers for broadcasters
    let _audio_producer;
    let _video_producer;

    if is_broadcaster {
        // Start audio producer
        _audio_producer = Some(AudioProducer::new(
            user_id.clone(),
            audio_data.clone(),
            packet_tx.clone(),
            media_start,
            loop_duration,
            is_speaking.clone(),
        )?);
        info!("Audio producer started for {}", user_id);

        // Start video producer
        let video_mode = &bot_config.video_mode;
        if *video_mode == VideoMode::Costume {
            if let Some(ref dir) = costume_dir {
                let renderer = CostumeRenderer::load(Path::new(dir))?;
                _video_producer = Some(VideoProducer::from_costume(
                    user_id.clone(),
                    renderer,
                    packet_tx.clone(),
                    media_start,
                    loop_duration,
                    is_speaking.clone(),
                )?);
                info!("Costume video producer started for {} ({})", user_id, dir);
            } else {
                // Costume mode but no costume_dir -- fall back to EKG
                warn!(
                    "[{}] video_mode=costume but no costume_dir set, falling back to EKG",
                    user_id
                );
                let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, 15);
                let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
                let renderer = EkgRenderer::new(ekg_color, 1280, 720);
                _video_producer = Some(VideoProducer::from_ekg(
                    user_id.clone(),
                    renderer,
                    rms,
                    max_rms,
                    packet_tx.clone(),
                    media_start,
                    loop_duration,
                )?);
                info!("EKG video producer started for {} (fallback)", user_id);
            }
        } else {
            // EKG mode
            let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, 15);
            let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
            let renderer = EkgRenderer::new(ekg_color, 1280, 720);
            _video_producer = Some(VideoProducer::from_ekg(
                user_id.clone(),
                renderer,
                rms,
                max_rms,
                packet_tx.clone(),
                media_start,
                loop_duration,
            )?);
            info!("EKG video producer started for {}", user_id);
        }
    } else {
        _audio_producer = None::<AudioProducer>;
        _video_producer = None::<VideoProducer>;
        info!("[{}] Observer mode -- no media producers", user_id);
    }

    info!("Client {} running", user_id);

    // Keep the client running
    tokio::signal::ctrl_c().await?;

    info!("Shutting down client: {}", user_id);

    quit.store(true, Ordering::Relaxed);
    client.stop().await;
    drop(_audio_producer);
    drop(_video_producer);

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
