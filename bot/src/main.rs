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
use bot::diagnostics_reporter::{spawn_diagnostics_reporter, DiagnosticsReporterConfig};
use bot::ekg_renderer::{self, EkgRenderer};
use bot::health_reporter::{spawn_health_reporter, HealthReporterConfig};
use bot::inbound_stats::InboundStats;
use bot::keyframe_requester::KeyframeRequester;
#[cfg(feature = "metrics")]
use bot::metrics_server::{self, BotMetrics};
use bot::netsim::{Admission, Direction, NetSimShim, NetworkProfile};
use bot::rtt_probe::spawn_rtt_probe;
use bot::transport::{self, OutboundFrame, TransportClient};
use bot::video_producer::VideoProducer;
use bot::websocket_client::spawn_heartbeat_producer;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};

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
                    // Default bind is loopback. Operators must pass
                    // `--metrics-bind 0.0.0.0` (or a specific NIC IP)
                    // explicitly to expose the endpoint on the network.
                    let bind = config
                        .metrics_bind
                        .unwrap_or(metrics_server::DEFAULT_METRICS_BIND);
                    metrics_server::start_server(Arc::clone(&registry), bind, port);
                    info!("Prometheus metrics listening on {bind}:{port}/metrics");
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
        if config.metrics_bind.is_some() {
            warn!(
                "--metrics-bind specified but the bot was built without `--features metrics`; \
                 the flag has no effect"
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
        let user_id_hook = user_id.clone();
        #[cfg(feature = "metrics")]
        let metrics_hook = metrics.clone();
        let hook: transport::InboundHook = Arc::new(move |payload| {
            if inbound_tx.try_send(payload).is_err() {
                // Overflow means the shim task is behind; degrade to drop so
                // we don't block the transport reader (that would create
                // head-of-line blocking back into the transport read loop).
                // Count the drop on a dedicated metric so silent inbound
                // loss can't compound the netsim loss model, and rate-limit
                // the warn! to avoid log-flooding under sustained overflow.
                static INBOUND_QUEUE_FULL_COUNT: AtomicU64 = AtomicU64::new(0);
                let count = INBOUND_QUEUE_FULL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count.is_multiple_of(100) || count == 1 {
                    warn!(
                        "[{}] netsim inbound queue full; dropping payload (total: {})",
                        user_id_hook, count
                    );
                }
                #[cfg(feature = "metrics")]
                if let Some(ref m) = metrics_hook {
                    m.netsim_dropped_total
                        .with_label_values(&[user_id_hook.as_str(), "down", "queue_full"])
                        .inc();
                }
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

    // The transport-facing packet channel. This carries raw wire bytes ready
    // to hand to the WebSocket/WebTransport sender. Producers upstream send
    // `OutboundFrame`s (bytes + media-type tag); the shim/counting task
    // below unwraps them and forwards `frame.bytes` here.
    let (transport_tx, transport_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task.
    client.start_packet_sender(transport_rx).await;

    // Outbound shim/counter task. We always splice in one task between
    // producers (which emit `OutboundFrame`) and the transport sender
    // (which consumes raw bytes), so the channel types don't need to be
    // conditional on feature / passthrough state.
    //
    // In passthrough + no-metrics the task body is a tiny forward loop;
    // with netsim enabled it applies the uplink impairment; with metrics
    // enabled it also labels Prometheus counters using the pre-tagged
    // `frame.kind` — no protobuf re-parse on the hot path.
    let (packet_tx, packet_rx) = mpsc::channel::<OutboundFrame>(500);

    // Shared counters for HealthPacket telemetry:
    // - packets_sent_counter: incremented by the outbound shim/passthrough on
    //   every successful transport send; read+reset by health reporter each tick.
    // - transport_drops_counter: cumulative try_send failures from any producer;
    //   read (not reset) by health reporter for websocket/datagram_drops_total.
    // - encoder_output_fps: written by the video producer with the current target
    //   FPS the encoder is configured at.
    let packets_sent_counter = Arc::new(AtomicU64::new(0));
    let transport_drops_counter = Arc::new(AtomicU64::new(0));
    let encoder_output_fps = Arc::new(AtomicU32::new(0));
    let encoder_errors_generic = Arc::new(AtomicU64::new(0));
    let encoder_frames_ok = Arc::new(AtomicU64::new(0));

    let outbound_shim_task = if network_profile.is_passthrough() {
        let user_id_out = user_id.clone();
        let transport_tx_inner = transport_tx.clone();
        let psc = packets_sent_counter.clone();
        #[cfg(feature = "metrics")]
        let metrics_out = metrics.clone();
        #[cfg(feature = "metrics")]
        let meeting_out = client_config.meeting_id.clone();
        let handle = tokio::spawn(run_outbound_passthrough(
            packet_rx,
            transport_tx_inner,
            user_id_out,
            psc,
            #[cfg(feature = "metrics")]
            metrics_out,
            #[cfg(feature = "metrics")]
            meeting_out,
        ));
        Some(handle)
    } else {
        let shim = NetSimShim::new(network_profile.clone(), Direction::Up);
        #[cfg(feature = "metrics")]
        let shim = match metrics.as_ref() {
            Some(m) => shim.with_metrics(Arc::clone(m), user_id.clone()),
            None => shim,
        };
        let shim = Arc::new(shim);
        let user_id_up = user_id.clone();
        let psc = packets_sent_counter.clone();
        #[cfg(feature = "metrics")]
        let metrics_up = metrics.clone();
        #[cfg(feature = "metrics")]
        let meeting_up = client_config.meeting_id.clone();
        let handle = tokio::spawn(run_outbound_shim(
            packet_rx,
            transport_tx,
            shim,
            user_id_up,
            psc,
            #[cfg(feature = "metrics")]
            metrics_up,
            #[cfg(feature = "metrics")]
            meeting_up,
        ));
        Some(handle)
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

    // --- RTT probe (passthrough bots only) ---
    // Impaired bots use simulated RTT (2× netsim latency); passthrough bots
    // send actual RTT probe packets to the relay and measure real round-trip.
    let (simulated_rtt_ms, measured_rtt_ms) = if network_profile.is_passthrough() {
        let rtt_state = spawn_rtt_probe(user_id.clone(), packet_tx.clone(), quit.clone());
        // Install the RTT probe state in InboundStats so echoed packets
        // are routed to record_echo instead of counted as media.
        {
            let mut s = stats.lock().unwrap();
            s.set_rtt_probe(Arc::clone(&rtt_state));
        }
        (None, Some(rtt_state.rtt_atomic()))
    } else {
        (Some((network_profile.latency_ms as f64) * 2.0), None)
    };

    // --- Keyframe requester ---
    // Send KEYFRAME_REQUEST to each newly discovered peer, mimicking browser
    // behavior on join.
    let keyframe_requests_sent = {
        let kr = KeyframeRequester::new(user_id.clone(), packet_tx.clone());
        let counter = kr.requests_sent_counter();
        {
            let mut s = stats.lock().unwrap();
            s.set_keyframe_requester(kr);
        }
        counter
    };

    // Spawn health reporter -- sends HealthPacket every 1s so senders can
    // observe this bot's received FPS and adjust their encoding tiers.
    spawn_health_reporter(
        HealthReporterConfig {
            client_config: client_config.clone(),
            transport: resolved_transport.clone(),
            simulated_rtt_ms,
            measured_rtt_ms,
            packets_sent_counter: packets_sent_counter.clone(),
            transport_drops_counter: transport_drops_counter.clone(),
            encoder_output_fps: encoder_output_fps.clone(),
            encoder_errors_generic: encoder_errors_generic.clone(),
            encoder_frames_ok: encoder_frames_ok.clone(),
            keyframe_requests_sent: Some(keyframe_requests_sent),
        },
        stats.clone(),
        packet_tx.clone(),
        quit.clone(),
        aq.clone(),
    );

    // Spawn the per-peer diagnostics reporter. Real browsers emit one
    // DiagnosticsPacket per observed remote peer per (audio, video) media
    // type every heartbeat; bots must do the same or sender-side AQ
    // controllers go blind in bot-heavy meetings. The reporter reads the
    // same 1s window as the health reporter via a non-destructive snapshot,
    // so counters are never double-drained.
    spawn_diagnostics_reporter(
        DiagnosticsReporterConfig {
            client_config: client_config.clone(),
            transport_drops_counter: transport_drops_counter.clone(),
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
                    encoder_output_fps.clone(),
                    encoder_errors_generic.clone(),
                    encoder_frames_ok.clone(),
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
                    encoder_output_fps.clone(),
                    encoder_errors_generic.clone(),
                    encoder_frames_ok.clone(),
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
                encoder_output_fps.clone(),
                encoder_errors_generic.clone(),
                encoder_frames_ok.clone(),
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

/// Outbound network-impairment task. Reads tagged [`OutboundFrame`]s from
/// producers, applies the uplink [`NetSimShim`] to the underlying bytes, and
/// forwards the bytes to the transport sender. Terminates when the producer
/// side closes.
///
/// When the `metrics` feature is enabled, `bot_packets_sent_total` is
/// incremented *before* the netsim shim makes its admission decision — the
/// counter reflects what producers offered to the uplink, not what actually
/// left the bot (drops are separately visible via `bot_netsim_dropped_total`).
///
/// The `media_type` Prometheus label comes directly from `frame.kind` — no
/// protobuf re-parse, just a `&'static str` lookup.
#[allow(clippy::too_many_arguments)]
async fn run_outbound_shim(
    mut rx: mpsc::Receiver<OutboundFrame>,
    tx: mpsc::Sender<Vec<u8>>,
    shim: Arc<NetSimShim>,
    user_id: String,
    packets_sent_counter: Arc<AtomicU64>,
    #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
    #[cfg(feature = "metrics")] meeting_id: String,
) {
    while let Some(frame) = rx.recv().await {
        #[cfg(feature = "metrics")]
        if let Some(ref m) = metrics {
            m.packets_sent_total
                .with_label_values(&[user_id.as_str(), meeting_id.as_str(), frame.kind.as_str()])
                .inc();
        }
        let payload = frame.bytes;
        let decision = shim.admit(payload.len());
        match decision {
            Admission::Pass => {
                if tx.send(payload).await.is_err() {
                    break;
                }
                packets_sent_counter.fetch_add(1, Ordering::Relaxed);
            }
            Admission::Drop => {
                debug!("[{}] netsim-up: dropped {}B", user_id, payload.len());
            }
            Admission::Delay(d) => {
                let tx = tx.clone();
                let psc = packets_sent_counter.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    if tx.send(payload).await.is_ok() {
                        psc.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            Admission::DelayAndDuplicate(d) => {
                let tx_a = tx.clone();
                let tx_b = tx.clone();
                let psc_a = packets_sent_counter.clone();
                let psc_b = packets_sent_counter.clone();
                let p_copy = payload.clone();
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    if tx_a.send(payload).await.is_ok() {
                        psc_a.fetch_add(1, Ordering::Relaxed);
                    }
                });
                tokio::spawn(async move {
                    if !d.is_zero() {
                        tokio::time::sleep(d).await;
                    }
                    if tx_b.send(p_copy).await.is_ok() {
                        psc_b.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        }
    }
    info!("Outbound netsim shim stopped for {}", user_id);
}

/// Outbound passthrough task used when network impairment is disabled.
/// Unwraps each [`OutboundFrame`] into raw bytes and forwards them to the
/// transport sender. When the `metrics` feature is enabled and a metrics
/// handle is present, it also increments `bot_packets_sent_total` using the
/// frame's pre-tagged media-type label.
#[allow(clippy::too_many_arguments)]
async fn run_outbound_passthrough(
    mut rx: mpsc::Receiver<OutboundFrame>,
    tx: mpsc::Sender<Vec<u8>>,
    user_id: String,
    packets_sent_counter: Arc<AtomicU64>,
    #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
    #[cfg(feature = "metrics")] meeting_id: String,
) {
    // `user_id` is used by the final info! log line; on metrics builds it's
    // also used as a Prometheus label. No dead-code warning either way.
    while let Some(frame) = rx.recv().await {
        #[cfg(feature = "metrics")]
        if let Some(ref m) = metrics {
            m.packets_sent_total
                .with_label_values(&[user_id.as_str(), meeting_id.as_str(), frame.kind.as_str()])
                .inc();
        }
        if tx.send(frame.bytes).await.is_err() {
            break;
        }
        packets_sent_counter.fetch_add(1, Ordering::Relaxed);
    }
    info!("Outbound passthrough stopped for {}", user_id);
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
