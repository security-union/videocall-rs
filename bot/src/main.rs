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

// All modules live in `src/lib.rs` so integration tests under `tests/`
// can share code with the binary. The binary only pulls in what it needs.
use bot::aq_controller::BotAq;
use bot::audio_producer::AudioProducer;
use bot::config::{self, BotConfig, ClientConfig, Manifest, Transport, VideoMode};
use bot::costume_renderer::CostumeRenderer;
use bot::ekg_renderer::{self, EkgRenderer};
use bot::health_reporter::{spawn_health_reporter, HealthReporterConfig};
use bot::inbound_stats::InboundStats;
#[cfg(feature = "metrics")]
use bot::metrics_server::{self, BotMetrics};
use bot::netsim::{Admission, Direction, NetSimShim, NetworkProfile};
use bot::transport::{self, TransportClient};
use bot::video_producer::VideoProducer;
use bot::websocket_client::spawn_heartbeat_producer;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};

/// Classify a serialized `PacketWrapper` into a stable `media_type` label
/// used by the bot_packets_* counters. Falls back to "unknown" on any parse
/// failure; we never fail a send because of this helper.
#[cfg(feature = "metrics")]
fn classify_outbound(payload: &[u8]) -> &'static str {
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::MediaPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    let Ok(wrapper) = PacketWrapper::parse_from_bytes(payload) else {
        return "unknown";
    };
    match wrapper.packet_type.enum_value() {
        Ok(PacketType::DIAGNOSTICS) => "diagnostics",
        Ok(PacketType::MEDIA) => match MediaPacket::parse_from_bytes(&wrapper.data) {
            Ok(m) => match m.media_type.enum_value() {
                Ok(MediaType::AUDIO) => "audio",
                Ok(MediaType::VIDEO) => "video",
                Ok(MediaType::HEARTBEAT) => "health",
                _ => "other",
            },
            Err(_) => "unknown",
        },
        _ => "other",
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Bridge the `log` crate (used by videocall-aq and other upstream crates)
    // into the tracing subscriber so AQ_STATUS / AQ_BITRATE_CHANGE / etc. show
    // up in the bot's log output alongside tracing events.
    if let Err(e) = tracing_log::LogTracer::init() {
        warn!("tracing_log::LogTracer::init failed: {} — log::* events from dependencies will not appear", e);
    }

    info!("Starting videocall synthetic client bot");

    let (config, num_users) = BotConfig::from_args()?;

    // Bring up the Prometheus metrics endpoint first so bots coming online
    // can publish their labels before the server starts accepting scrapes.
    // Zero-cost compile-out when the `metrics` feature is off.
    #[cfg(feature = "metrics")]
    let metrics_handle: Option<Arc<BotMetrics>> = match config.metrics_port {
        Some(port) => {
            let registry = Arc::new(prometheus::Registry::new());
            match BotMetrics::new(Arc::clone(&registry)) {
                Ok(handle) => {
                    metrics_server::start_server(Arc::clone(&registry), port);
                    info!("Prometheus metrics listening on :{port}/metrics");
                    Some(handle)
                }
                Err(e) => {
                    warn!("Failed to register bot metrics: {e} — metrics disabled");
                    None
                }
            }
        }
        None => None,
    };
    #[cfg(not(feature = "metrics"))]
    {
        if config.metrics_port.is_some() {
            warn!(
                "--metrics-port specified but the bot was built without `--features metrics`; \
                 the endpoint will NOT be started"
            );
        }
    }
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

    // Pre-flight memory check for costume video mode
    if config.video_mode == VideoMode::Costume {
        let mut total_costume_bytes: u64 = 0;
        let mut costume_count = 0usize;
        for p in active_participants
            .iter()
            .filter(|p| broadcaster_names.contains(p.name.as_str()))
        {
            if let Some(ref dir) = p.costume_dir {
                let idle_path = format!("{dir}/idle.i420");
                let talking_path = format!("{dir}/talking.i420");
                if let (Ok(idle_meta), Ok(talk_meta)) = (
                    std::fs::metadata(&idle_path),
                    std::fs::metadata(&talking_path),
                ) {
                    total_costume_bytes += idle_meta.len() + talk_meta.len();
                    costume_count += 1;
                }
            }
        }
        if costume_count > 0 {
            let total_gb = total_costume_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            info!(
                "Costume memory estimate: {:.1} GiB for {} costumes",
                total_gb, costume_count
            );
            if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                if let Some(avail_line) = meminfo.lines().find(|l| l.starts_with("MemAvailable:")) {
                    if let Some(kb_str) = avail_line.split_whitespace().nth(1) {
                        if let Ok(avail_kb) = kb_str.parse::<u64>() {
                            let avail_bytes = avail_kb * 1024;
                            if total_costume_bytes > avail_bytes * 80 / 100 {
                                warn!(
                                    "Costume frames ({:.1} GiB) exceed 80% of available memory ({:.1} GiB) — risk of OOM",
                                    total_gb,
                                    avail_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

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

        // Resolve network profile for this participant once, upfront, so
        // invalid configs fail the whole run before we spawn transports.
        let network_profile = config.resolve_network(p)?;
        if !network_profile.is_passthrough() {
            info!(
                "[{}] network impairment: latency={}ms jitter={}ms loss={}% up={:?}kbps down={:?}kbps",
                p.name,
                network_profile.latency_ms,
                network_profile.jitter_ms,
                network_profile.loss_pct,
                network_profile.uplink_kbps,
                network_profile.downlink_kbps,
            );
        }

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
        let netprof = network_profile;
        #[cfg(feature = "metrics")]
        let metrics_for_bot = metrics_handle.clone();

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
                netprof,
                #[cfg(feature = "metrics")]
                metrics_for_bot,
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
    network_profile: NetworkProfile,
    #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
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

    // Adaptive-quality controller, created before any producers so they can
    // read the initial tier snapshot on start.
    let aq = BotAq::with_default_clock();
    #[cfg(feature = "metrics")]
    if let Some(ref m) = metrics {
        aq.set_metrics(
            Arc::clone(m),
            user_id.clone(),
            client_config.meeting_id.clone(),
        );
    }

    // Shared inbound stats -- used by both the transport's inbound consumer
    // and the health reporter for per-sender packet rate tracking. We also
    // wire the AQ controller here so incoming DIAGNOSTICS packets get fed
    // straight into the PID loop.
    let stats = Arc::new(Mutex::new(InboundStats::default()));
    {
        let mut s = stats.lock().unwrap();
        s.set_aq(aq.clone());
        #[cfg(feature = "metrics")]
        if let Some(ref m) = metrics {
            s.set_metrics(
                Arc::clone(m),
                user_id.clone(),
                client_config.meeting_id.clone(),
            );
        }
    }

    // Shared is_speaking flag -- audio producer sets, heartbeat/video reads
    let is_speaking = Arc::new(AtomicBool::new(false));

    // Construct the inbound shim (if any) before connecting, so the hook is
    // installed as the transport comes up and we don't race the first packet.
    let (inbound_hook, inbound_shim_task) = if network_profile.is_passthrough() {
        (None, None)
    } else {
        let shim = NetSimShim::new(network_profile.clone(), Direction::Down);
        #[cfg(feature = "metrics")]
        let shim = match metrics.as_ref() {
            Some(m) => shim.with_metrics(Arc::clone(m), user_id.clone()),
            None => shim,
        };
        let shim = Arc::new(shim);
        // Buffer of 2048 matches order-of-magnitude sizing used elsewhere —
        // at 100 pkts/sec with up to a few seconds of queuing, this is safe
        // without being large enough to cause unbounded memory growth.
        let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(2048);
        let user_id_dn = user_id.clone();
        let stats_dn = stats.clone();
        let shim_dn = shim.clone();
        let handle = tokio::spawn(run_inbound_shim(inbound_rx, shim_dn, stats_dn, user_id_dn));
        let hook: transport::InboundHook = Arc::new(move |payload| {
            if inbound_tx.try_send(payload).is_err() {
                // Overflow means the shim task is behind; degrade to drop so
                // we don't block the transport reader.
                debug!("netsim inbound queue full; dropping payload");
            }
        });
        (Some(hook), Some(handle))
    };

    let mut client = TransportClient::new(&resolved_transport, client_config.clone());
    client
        .connect(
            &lobby_url,
            insecure,
            stats.clone(),
            is_speaking.clone(),
            inbound_hook,
        )
        .await?;

    // The transport-facing packet channel. Its sender is either handed
    // directly to producers (passthrough) or fed by the uplink shim task.
    let (transport_tx, transport_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task.
    client.start_packet_sender(transport_rx).await;

    // Outbound impairment: insert a shim task between producers and the
    // transport sender. Passthrough bots get the transport sender directly.
    let (packet_tx, outbound_shim_task) = if network_profile.is_passthrough() {
        #[cfg(feature = "metrics")]
        {
            // In passthrough mode, we still want per-media-type send counters.
            // Splice in a counting task so the transport sender sees every
            // packet once, labeled by type.
            if let Some(m) = metrics.clone() {
                let (counter_tx, mut counter_rx) = mpsc::channel::<Vec<u8>>(500);
                let user_id_ctr = user_id.clone();
                let meeting_ctr = client_config.meeting_id.clone();
                let transport_tx_inner = transport_tx.clone();
                let metrics_for_task = m;
                let handle = tokio::spawn(async move {
                    while let Some(payload) = counter_rx.recv().await {
                        let mt = classify_outbound(&payload);
                        metrics_for_task
                            .packets_sent_total
                            .with_label_values(&[user_id_ctr.as_str(), meeting_ctr.as_str(), mt])
                            .inc();
                        if transport_tx_inner.send(payload).await.is_err() {
                            break;
                        }
                    }
                });
                (counter_tx, Some(handle))
            } else {
                (transport_tx, None)
            }
        }
        #[cfg(not(feature = "metrics"))]
        {
            (transport_tx, None)
        }
    } else {
        let (producer_tx, producer_rx) = mpsc::channel::<Vec<u8>>(500);
        let shim = NetSimShim::new(network_profile.clone(), Direction::Up);
        #[cfg(feature = "metrics")]
        let shim = match metrics.as_ref() {
            Some(m) => shim.with_metrics(Arc::clone(m), user_id.clone()),
            None => shim,
        };
        let shim = Arc::new(shim);
        let user_id_up = user_id.clone();
        #[cfg(feature = "metrics")]
        let metrics_up = metrics.clone();
        #[cfg(feature = "metrics")]
        let meeting_up = client_config.meeting_id.clone();
        let handle = tokio::spawn(run_outbound_shim(
            producer_rx,
            transport_tx,
            shim,
            user_id_up,
            #[cfg(feature = "metrics")]
            metrics_up,
            #[cfg(feature = "metrics")]
            meeting_up,
        ));
        (producer_tx, Some(handle))
    };

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
        aq.clone(),
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
            aq.clone(),
        )?);
        info!("Audio producer started for {}", user_id);

        // Start video producer. The initial snapshot from `aq` already
        // reflects the default tier; producers poll `aq.tier_epoch()` each
        // iteration and re-snapshot on change.
        let v0 = aq.snapshot_video();
        let ekg_width = v0.max_width;
        let ekg_height = v0.max_height;
        let ekg_fps = v0.target_fps.max(1);
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
                    aq.clone(),
                )?);
                info!("Costume video producer started for {} ({})", user_id, dir);
            } else {
                // Costume mode but no costume_dir -- fall back to EKG.
                warn!(
                    "[{}] video_mode=costume but no costume_dir set, falling back to EKG",
                    user_id
                );
                let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, ekg_fps);
                let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
                let renderer = EkgRenderer::new(ekg_color, ekg_width, ekg_height);
                _video_producer = Some(VideoProducer::from_ekg(
                    user_id.clone(),
                    renderer,
                    rms,
                    max_rms,
                    packet_tx.clone(),
                    media_start,
                    loop_duration,
                    aq.clone(),
                )?);
                info!("EKG video producer started for {} (fallback)", user_id);
            }
        } else {
            // EKG mode
            let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, ekg_fps);
            let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
            let renderer = EkgRenderer::new(ekg_color, ekg_width, ekg_height);
            _video_producer = Some(VideoProducer::from_ekg(
                user_id.clone(),
                renderer,
                rms,
                max_rms,
                packet_tx.clone(),
                media_start,
                loop_duration,
                aq.clone(),
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

    // Let shim tasks drain. They terminate when their input channel closes,
    // which happens when the producer side is dropped (outbound) or the
    // hook is dropped (inbound). A short timeout prevents hangs if a sleep
    // is still in flight.
    if let Some(h) = outbound_shim_task {
        let _ = tokio::time::timeout(Duration::from_secs(3), h).await;
    }
    if let Some(h) = inbound_shim_task {
        let _ = tokio::time::timeout(Duration::from_secs(3), h).await;
    }

    info!("Client {} shut down cleanly", user_id);
    Ok(())
}

/// Outbound network-impairment task. Reads raw wire-formatted frames from
/// producers, applies the uplink [`NetSimShim`], and forwards to the
/// transport sender. Terminates when the producer side closes.
///
/// When the `metrics` feature is enabled, `bot_packets_sent_total` is
/// incremented *before* the netsim shim makes its admission decision — the
/// counter reflects what producers offered to the uplink, not what actually
/// left the bot (drops are separately visible via `bot_netsim_dropped_total`).
#[allow(clippy::too_many_arguments)]
async fn run_outbound_shim(
    mut rx: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Vec<u8>>,
    shim: Arc<NetSimShim>,
    user_id: String,
    #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
    #[cfg(feature = "metrics")] meeting_id: String,
) {
    while let Some(payload) = rx.recv().await {
        #[cfg(feature = "metrics")]
        if let Some(ref m) = metrics {
            let mt = classify_outbound(&payload);
            m.packets_sent_total
                .with_label_values(&[user_id.as_str(), meeting_id.as_str(), mt])
                .inc();
        }
        let decision = shim.admit(payload.len());
        match decision {
            Admission::Pass => {
                if tx.send(payload).await.is_err() {
                    break;
                }
            }
            Admission::Drop => {
                debug!("[{}] netsim-up: dropped {}B", user_id, 0);
            }
            Admission::Delay(d) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let _ = tx.send(payload).await;
                });
            }
            Admission::DelayAndDuplicate(d) => {
                let tx_a = tx.clone();
                let tx_b = tx.clone();
                let p_copy = payload.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let _ = tx_a.send(payload).await;
                });
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let _ = tx_b.send(p_copy).await;
                });
            }
        }
    }
    info!("Outbound netsim shim stopped for {}", user_id);
}

/// Inbound network-impairment task. Receives payloads the transport readers
/// delivered via the `InboundHook`, applies the downlink [`NetSimShim`], and
/// (after any delay) hands the bytes to [`InboundStats::record_packet`].
async fn run_inbound_shim(
    mut rx: mpsc::Receiver<Vec<u8>>,
    shim: Arc<NetSimShim>,
    stats: Arc<Mutex<InboundStats>>,
    user_id: String,
) {
    while let Some(payload) = rx.recv().await {
        let decision = shim.admit(payload.len());
        match decision {
            Admission::Pass => {
                let mut s = stats.lock().unwrap();
                s.record_packet(&user_id, &payload);
            }
            Admission::Drop => {
                debug!("[{}] netsim-down: dropped {}B", user_id, payload.len());
            }
            Admission::Delay(d) => {
                let stats = stats.clone();
                let user_id = user_id.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let mut s = stats.lock().unwrap();
                    s.record_packet(&user_id, &payload);
                });
            }
            Admission::DelayAndDuplicate(d) => {
                let stats_a = stats.clone();
                let user_id_a = user_id.clone();
                let stats_b = stats.clone();
                let user_id_b = user_id.clone();
                let p_copy = payload.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let mut s = stats_a.lock().unwrap();
                    s.record_packet(&user_id_a, &payload);
                });
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    let mut s = stats_b.lock().unwrap();
                    s.record_packet(&user_id_b, &p_copy);
                });
            }
        }
    }
    info!("Inbound netsim shim stopped for {}", user_id);
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
