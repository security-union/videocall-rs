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
use bot::audio_producer::{stitch_participant_audio, AudioProducer};
use bot::config::{
    self, evaluate_costume_memory, BotConfig, ClientConfig, CostumeMemoryDecision, Manifest,
    Transport, VideoMode,
};
use bot::costume_renderer::CostumeRenderer;
use bot::diagnostics_reporter::{spawn_diagnostics_reporter, DiagnosticsReporterConfig};
use bot::ekg_renderer::{self, EkgRenderer};
use bot::health_reporter::{spawn_health_reporter, HealthReporterConfig};
use bot::inbound_stats::InboundStats;
use bot::keyframe_requester::KeyframeRequester;
use bot::layer_preference_sender::LayerPreferenceSender;
#[cfg(feature = "metrics")]
use bot::metrics_server::{self, BotMetrics};
use bot::netsim::{Admission, Direction, NetSimShim, NetworkProfile};
use bot::rtt_probe::spawn_rtt_probe;
use bot::run_manifest;
use bot::shutdown;
use bot::transport::{
    self, OutboundFrame, OutboundFrameSender, TransportClient, WebSocketStreamByteCounters,
};
use bot::video_producer::VideoProducer;
use bot::viewport_sender::ViewportSender;
use bot::websocket_client::spawn_heartbeat_producer;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use videocall_meeting_types::mint::LobbyAuth;
use videocall_types::url_log::strip_query_for_log;

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
    let run_started_at = run_manifest::epoch_secs();

    // One shutdown flag for the whole run: Ctrl-C, SIGTERM or --duration.
    let (shutdown_tx, mut shutdown_rx) = shutdown::shutdown_channel();
    let run_duration = config.run_duration()?;
    let _shutdown_trigger = shutdown::spawn_shutdown_trigger(shutdown_tx, run_duration);
    if let Some(d) = run_duration {
        info!("Run duration: {:?} (from process start)", d);
    }

    // Fail before any bot is spawned rather than once per client (#2298).
    config.resolve_lobby_auth()?;

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

    // Roles follow the run-wide roster position (config::build_roster): generated
    // bot-NNN participants are cameras or viewers, never audio publishers.
    let roster_offset = config.roster_offset.unwrap_or(0);
    let speaker_set: HashSet<&str> = manifest.lines.iter().map(|l| l.speaker.as_str()).collect();
    let roster = config::build_roster(
        &manifest.participants,
        &speaker_set,
        roster_offset,
        num_users,
        &config.population(),
    );
    let n = roster.len();
    if n == 0 {
        return Err(anyhow::anyhow!(
            "no participants to run (empty manifest and --users 0, or --roster-offset past it)"
        ));
    }
    let active_participants: Vec<&config::Participant> =
        roster.iter().map(|e| &e.participant).collect();
    let role_of: HashMap<&str, config::Role> = roster
        .iter()
        .map(|e| (e.participant.name.as_str(), e.role))
        .collect();
    let broadcaster_names: HashSet<&str> = roster
        .iter()
        .filter(|e| e.role.sends_audio())
        .map(|e| e.participant.name.as_str())
        .collect();

    info!(
        "Active participants ({}): {}",
        n,
        roster
            .iter()
            .map(|e| format!("{} ({:?})", e.participant.name, e.role))
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

    // Stitch per-broadcaster audio (plain conversation audio, no warmup padding).
    // Receive-only participants get no timeline: they never produce audio.
    let pause_samples = (manifest.pause_ms as usize * 48000) / 1000;
    let speakers: Vec<&str> = active_lines.iter().map(|l| l.speaker.as_str()).collect();
    let broadcaster_list: Vec<&str> = active_participants
        .iter()
        .map(|p| p.name.as_str())
        .filter(|name| broadcaster_names.contains(name))
        .collect();
    let (mut participant_audio, total_samples) =
        stitch_participant_audio(&speakers, &line_audio, &broadcaster_list, pause_samples);

    // Network profile of every participant this process runs, keyed by wire
    // user id, so receivers can label delay by sender profile.
    let sender_profiles: Arc<HashMap<String, String>> = Arc::new(
        active_participants
            .iter()
            .map(|p| (config.wire_user_id(&p.name), config.network_label(p)))
            .collect(),
    );
    // The bot's share of the run manifest (Discussion #2913 §5.2): one record
    // per participant, completed with actual join/leave times as the run goes.
    let mut records = Vec::with_capacity(n);
    for (index, entry) in roster.iter().enumerate() {
        let p = &entry.participant;
        let (transport, _) = config.resolve_transport(roster_offset + index)?;
        records.push(run_manifest::ParticipantRecord {
            user_id: config.wire_user_id(&p.name),
            fleet: "rust",
            role: run_manifest::role_for(entry.role),
            observer: false,
            talker: entry.role.is_talker(),
            publishes: run_manifest::Publishes {
                camera: entry.role.sends_video(),
                mic: entry.role.sends_audio(),
                screen: false,
            },
            network: run_manifest::network_record(
                &config.network_label(p),
                &config.resolve_network(p)?,
            ),
            transport_intended: match transport {
                Transport::WebSocket => "websocket",
                Transport::WebTransport => "webtransport",
            },
            placement: config
                .placement_node
                .clone()
                .map(|node| run_manifest::Placement { node }),
            join_ts: None,
            leave_ts: None,
            instance_id: None,
            outcome: None,
        });
    }
    let registry = Arc::new(run_manifest::ParticipantRegistry::new(
        config.meeting_id.clone(),
        config.id_prefix.clone(),
        run_started_at,
        records,
    ));
    let participants_out = config
        .participants_out
        .as_ref()
        .map(std::path::PathBuf::from);
    if let Some(path) = &participants_out {
        registry.write(path)?;
        info!("Participant list written to {}", path.display());
    }
    let participants_writer = participants_out.clone().map(|path| {
        run_manifest::ParticipantsWriter::spawn(
            Arc::clone(&registry),
            path,
            run_manifest::WRITE_INTERVAL,
        )
    });

    let loop_duration = config::media_loop_duration(total_samples);
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
            .filter(|p| role_of[p.name.as_str()].sends_video())
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
                            let avail_gb = avail_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                            match evaluate_costume_memory(
                                total_costume_bytes,
                                avail_bytes,
                                config.strict_memory,
                            ) {
                                CostumeMemoryDecision::AbortExceedsAvailable => {
                                    error!(
                                        "Costume frames ({:.1} GiB) exceed available memory ({:.1} GiB) — aborting",
                                        total_gb, avail_gb
                                    );
                                    std::process::exit(1);
                                }
                                CostumeMemoryDecision::AbortStrictThreshold => {
                                    error!(
                                        "Costume frames ({:.1} GiB) exceed 80% of available memory ({:.1} GiB) — aborting (--strict-memory)",
                                        total_gb, avail_gb
                                    );
                                    std::process::exit(1);
                                }
                                CostumeMemoryDecision::Warn => {
                                    warn!(
                                        "Costume frames ({:.1} GiB) exceed 80% of available memory ({:.1} GiB) — risk of OOM (pass --strict-memory to abort)",
                                        total_gb, avail_gb
                                    );
                                }
                                CostumeMemoryDecision::Ok => {}
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
        if *shutdown_rx.borrow() {
            warn!(
                "Shutdown requested during ramp-up; {} of {} clients started",
                index, n
            );
            break;
        }
        let audio_data = participant_audio.remove(&p.name).unwrap_or_default();
        let role = role_of[p.name.as_str()];
        let rx_profile = config.network_label(p);
        let profiles = Arc::clone(&sender_profiles);
        let client_registry = Arc::clone(&registry);

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
            "Starting client {} ({}) - audio: {} samples, role: {:?}",
            index,
            p.name,
            audio_data.len(),
            role,
        );

        let bot_config = config.clone();
        let user_id = config.wire_user_id(&p.name);
        let meeting_id = config.meeting_id.clone();
        let ekg_color = p.ekg_color;
        let costume_dir = p.costume_dir.clone();
        let cell = media_start_cell.clone();
        let ld = loop_duration;
        let position = roster_offset + index;
        let netprof = network_profile;
        #[cfg(feature = "metrics")]
        let metrics_for_bot = metrics_handle.clone();

        let client_shutdown = shutdown_rx.clone();
        let handle = tokio::spawn(async move {
            match run_client(
                bot_config,
                user_id,
                meeting_id,
                audio_data,
                ekg_color,
                costume_dir,
                insecure,
                cell,
                ld,
                position,
                role,
                netprof,
                rx_profile,
                profiles,
                client_registry,
                client_shutdown,
                #[cfg(feature = "metrics")]
                metrics_for_bot,
            )
            .await
            {
                Ok(()) => true,
                Err(e) => {
                    error!("Client failed: {}", e);
                    false
                }
            }
        });

        client_handles.push(handle);

        if index < n - 1 {
            info!(
                "Waiting {}ms before starting next client",
                ramp_up_delay.as_millis()
            );
            shutdown::sleep_or_shutdown(ramp_up_delay, &mut shutdown_rx).await;
        }
    }

    // All bots spawned -- wait warmup then start media
    let warmup = config.warmup_secs();
    info!(
        "All {} clients spawned, waiting {}s warmup before starting media",
        n, warmup
    );
    shutdown::sleep_or_shutdown(Duration::from_secs(warmup), &mut shutdown_rx).await;

    let now = shutdown::release_media(&media_start_cell, &registry);
    info!("Media start signal sent at {:?}", now);

    info!(
        "All {} clients running until {}",
        n,
        if run_duration.is_some() {
            "the run duration elapses, Ctrl-C or SIGTERM"
        } else {
            "Ctrl-C or SIGTERM"
        }
    );

    let started = client_handles.len();
    let mut failed = 0usize;
    let mut stop_deadline = None;
    for handle in client_handles {
        let joined = shutdown::join_or_abort(
            handle,
            &mut shutdown_rx,
            CLIENT_STOP_GRACE,
            &mut stop_deadline,
        )
        .await;
        if joined != Some(true) {
            failed += 1;
        }
    }

    info!(
        "All clients finished: {} started, {} failed",
        started, failed
    );
    let interim_writes = registry
        .close(
            participants_writer,
            participants_out.as_deref(),
            run_manifest::epoch_secs(),
        )
        .await?;
    if let Some(path) = &participants_out {
        info!(
            "Participant list finalized in {} after {} interim writes",
            path.display(),
            interim_writes
        );
    }
    if failed > 0 {
        return Err(anyhow::anyhow!("{failed} of {started} clients failed"));
    }
    Ok(())
}

/// How long clients get to stop after shutdown before they are aborted.
const CLIENT_STOP_GRACE: Duration = Duration::from_secs(30);

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
    position: usize,
    role: config::Role,
    network_profile: NetworkProfile,
    rx_profile: String,
    sender_profiles: Arc<HashMap<String, String>>,
    registry: Arc<run_manifest::ParticipantRegistry>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    #[cfg(feature = "metrics")] metrics: Option<Arc<BotMetrics>>,
) -> anyhow::Result<()> {
    info!("Initializing client: {} (role={:?})", user_id, role);

    // Resolve transport for this bot
    let (resolved_transport, server_url) = bot_config.resolve_transport(position)?;

    let timings = bot_config.control_timings()?;
    let client_config =
        ClientConfig::for_role(user_id.clone(), meeting_id, role, timings.heartbeat);

    let lobby_auth = bot_config.resolve_lobby_auth()?;
    let lobby_url = TransportClient::build_lobby_url(
        &resolved_transport,
        &server_url,
        &lobby_auth,
        &client_config.user_id,
        &client_config.meeting_id,
    )?;
    // Like the browser, identify this client instance so the relay's
    // reconnect-grace and stale-session paths key on it. The relay ignores it on
    // the deprecated path-based join, so it is only sent with a token.
    let instance_id = config::generate_instance_id(&mut rand::thread_rng());
    let lobby_url = if matches!(lobby_auth, LobbyAuth::DeprecatedPath) {
        lobby_url
    } else {
        registry.set_instance_id(&user_id, &instance_id);
        config::append_instance_id(lobby_url, &instance_id)
    };
    info!(
        "[{}] Transport: {:?}, Lobby URL: {}{}",
        user_id,
        resolved_transport,
        strip_query_for_log(lobby_url.as_str()),
        if lobby_url.query().is_some() {
            "?<redacted>"
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

    // Simulcast AQ wiring (issue #1083 V21): when this bot publishes a
    // multi-layer ladder (--simulcast-layers N>=2), enable simulcast on the AQ
    // controller so its per-layer budget cap (`cap_layers_to_budget`) and
    // top-layer shed paths are reachable. The bot starts at the FULL ladder
    // (shed-only) — see `BotAq::set_simulcast_layers` for the deliberate
    // divergence from the browser's start-at-base ramp. No-op for N<2 or when
    // video is disabled (observer / audio-only bots never publish video layers).
    let simulcast_layer_count = bot_config.simulcast_layer_count();
    if client_config.enable_video && simulcast_layer_count >= 2 {
        aq.set_simulcast_layers(simulcast_layer_count as usize);
    }

    // Shared inbound stats -- used by both the transport's inbound consumer
    // and the health reporter for per-sender packet rate tracking.
    //
    // NOTE(#1108): the AQ controller is no longer wired into inbound stats —
    // receiver-reported DIAGNOSTICS no longer feed the sender AQ. The AQ now
    // advances on a self-timer (see the `aq.tick()` task spawned below).
    let stats = Arc::new(Mutex::new(InboundStats::default()));
    if client_config.enable_video {
        stats.lock().unwrap().set_layer_hint_aq(Arc::clone(&aq));
    }
    #[cfg(feature = "metrics")]
    {
        let mut s = stats.lock().unwrap();
        if let Some(ref m) = metrics {
            s.set_metrics(
                Arc::clone(m),
                user_id.clone(),
                client_config.meeting_id.clone(),
            );
            s.set_delay_labels(bot::inbound_stats::DelayLabels {
                transport: match resolved_transport {
                    Transport::WebSocket => "websocket",
                    Transport::WebTransport => "webtransport",
                },
                rx_profile: rx_profile.clone(),
                sender_profiles: Arc::clone(&sender_profiles),
            });
        }
    }
    #[cfg(not(feature = "metrics"))]
    let _ = (&rx_profile, &sender_profiles);

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

    let mut client = TransportClient::new(
        &resolved_transport,
        client_config.clone(),
        #[cfg(feature = "metrics")]
        metrics.clone(),
    );
    client
        .connect(
            &lobby_url,
            insecure,
            stats.clone(),
            is_speaking.clone(),
            inbound_hook,
        )
        .await?;
    let presence = registry.join(&user_id, run_manifest::epoch_secs());

    // The transport-facing packet channel. This carries raw wire bytes ready
    // to hand to the WebSocket/WebTransport sender. Producers upstream send
    // `OutboundFrame`s (bytes + media-type tag); the shim/counting task
    // below unwraps them and forwards `frame.bytes` here.
    let (transport_tx, transport_rx) = mpsc::channel::<Vec<u8>>(500);

    // Start packet sender task.
    client.start_packet_sender(transport_rx).await;
    let mut closed = client.closed();
    let (shutdown_peek, closed_peek) = (shutdown_rx.clone(), closed.clone());
    let quit = Arc::new(AtomicBool::new(false));
    let (started, result) = shutdown::hold_then_stop(
        &mut client,
        &quit,
        async {
            // Outbound shim/counter task. We always splice in one task between
            // producers (which emit `OutboundFrame`) and the transport sender
            // (which consumes raw bytes), so the channel types don't need to be
            // conditional on feature / passthrough state.
            //
            // In passthrough + no-metrics the task body is a tiny forward loop;
            // with netsim enabled it applies the uplink impairment; with metrics
            // enabled it also labels Prometheus counters using the pre-tagged
            // `frame.kind` — no protobuf re-parse on the hot path.
            let (packet_tx_raw, packet_rx) = mpsc::channel::<OutboundFrame>(500);
            // `Some` only on a WebSocket run: one Option both wires the byte accounting
            // and reaches the health reporter, so the two cannot disagree on transport.
            let websocket_stream_bytes = matches!(resolved_transport, Transport::WebSocket)
                .then(|| Arc::new(WebSocketStreamByteCounters::default()));
            let packet_tx = match websocket_stream_bytes.clone() {
                Some(counters) => {
                    OutboundFrameSender::with_websocket_accounting(packet_tx_raw, counters)
                }
                None => OutboundFrameSender::new(packet_tx_raw),
            };

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

            // Handle to the uplink netsim shim, shared with the AQ tick so it can read
            // the shim's `bandwidth_wait_us` saturation counter (issue #1083 V21).
            // `None` in passthrough (no shim runs), so the AQ sees zero uplink
            // saturation and the legacy zero-backpressure behavior is preserved.
            let mut uplink_shim: Option<Arc<NetSimShim>> = None;

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
                // Share the uplink shim with the AQ tick (issue #1083 V21): the tick
                // reads `bandwidth_wait_us` to detect the bot's own uplink saturation.
                uplink_shim = Some(shim.clone());
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

            {
                let aq_tick = aq.clone();
                let quit_tick = quit.clone();
                let uplink_shim_tick = uplink_shim.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_millis(
                        videocall_aq::constants::AQ_TICK_INTERVAL_MS,
                    ));
                    loop {
                        interval.tick().await;
                        if quit_tick.load(Ordering::Relaxed) {
                            break;
                        }
                        let uplink_wait_us = uplink_shim_tick
                            .as_ref()
                            .map(|s| s.bandwidth_wait_us())
                            .unwrap_or(0);
                        aq_tick.observe_uplink_saturation(uplink_wait_us);
                        aq_tick.tick();
                    }
                });
            }

            // For WebSocket transport, heartbeats go through the shared mpsc channel
            if matches!(resolved_transport, Transport::WebSocket) {
                spawn_heartbeat_producer(
                    client_config.clone(),
                    packet_tx.clone(),
                    quit.clone(),
                    is_speaking.clone(),
                );
            }

            // --- RTT probe (passthrough bots only) ---
            // Passthrough bots probe the relay for a real RTT; impaired bots report none.
            let measured_rtt_ms = if network_profile.is_passthrough() {
                let rtt_state = spawn_rtt_probe(user_id.clone(), packet_tx.clone(), quit.clone());
                // Install the RTT probe state in InboundStats so echoed packets
                // are routed to record_echo instead of counted as media.
                {
                    let mut s = stats.lock().unwrap();
                    s.set_rtt_probe(Arc::clone(&rtt_state));
                }
                Some(rtt_state.rtt_atomic())
            } else {
                None
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

            // --- Viewport sender (HCL issue #988) ---
            {
                let vs = ViewportSender::new(
                    user_id.clone(),
                    bot_config.viewport_visible_count,
                    packet_tx.clone(),
                );
                if vs.is_enabled() {
                    info!(
                        "[{}] VIEWPORT fidelity enabled: rendering up to {:?} peer(s)",
                        user_id, bot_config.viewport_visible_count
                    );
                }
                let mut s = stats.lock().unwrap();
                s.set_viewport_sender(vs);
            }

            // --- Layer-preference sender (HCL follow-up #1083-A2) ---
            {
                let lps = LayerPreferenceSender::new(
                    user_id.clone(),
                    bot_config.pin_layer,
                    bot_config.pin_media_kind(),
                    packet_tx.clone(),
                );
                if lps.is_enabled() {
                    info!(
                    "[{}] LAYER_PREFERENCE pin enabled: pinning every source to layer {:?} ({:?})",
                    user_id,
                    bot_config.pin_layer,
                    bot_config.pin_media_kind()
                );
                }
                let mut s = stats.lock().unwrap();
                s.set_layer_preference_sender(lps);
            }

            spawn_health_reporter(
                HealthReporterConfig {
                    client_config: client_config.clone(),
                    interval: timings.health,
                    transport: resolved_transport.clone(),
                    measured_rtt_ms,
                    packets_sent_counter: packets_sent_counter.clone(),
                    transport_drops_counter: transport_drops_counter.clone(),
                    websocket_stream_bytes: websocket_stream_bytes.clone(),
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

            // Spawn the per-peer diagnostics reporter on its own cadence.
            spawn_diagnostics_reporter(
                DiagnosticsReporterConfig {
                    client_config: client_config.clone(),
                    interval: timings.diagnostics,
                    persistent_trackers: timings.persistent_diagnostics,
                    max_video_trackers: bot_config.diag_video_tracker_cap(),
                    enabled: bot_config.diagnostics_enabled(),
                    transport_drops_counter: transport_drops_counter.clone(),
                },
                stats,
                packet_tx.clone(),
                quit.clone(),
            );

            let media_start =
                shutdown::wait_for_media_start(&media_start_cell, &shutdown_peek, &closed_peek)
                    .await;

            let mut audio_producer: Option<AudioProducer> = None;
            let mut video_producer: Option<VideoProducer> = None;
            if let Some(media_start) = media_start {
                if role.sends_video() {
                    audio_producer = role
                        .sends_audio()
                        .then(|| {
                            AudioProducer::new(
                                user_id.clone(),
                                audio_data.clone(),
                                packet_tx.clone(),
                                media_start,
                                loop_duration,
                                is_speaking.clone(),
                                aq.clone(),
                                transport_drops_counter.clone(),
                                role.is_talker(),
                            )
                        })
                        .transpose()?;
                    info!("Media producers starting for {} ({:?})", user_id, role);

                    // Start video producer. The initial snapshot from `aq` already
                    // reflects the default tier; producers poll `aq.tier_epoch()` each
                    // iteration and re-snapshot on change.
                    let v0 = aq.snapshot_video();
                    let ekg_width = v0.max_width;
                    let ekg_height = v0.max_height;
                    let ekg_fps = v0.target_fps.max(1);
                    let video_mode = &bot_config.video_mode;
                    // Resolved simulcast layer count (#989): default 3, clamped to
                    // 1..=SIMULCAST_MAX_LAYERS. N==1 = legacy single-stream path.
                    let simulcast_layers = bot_config.simulcast_layer_count();
                    if *video_mode == VideoMode::Costume {
                        if let Some(ref dir) = costume_dir {
                            let renderer = CostumeRenderer::load(Path::new(dir))?;
                            video_producer = Some(VideoProducer::from_costume(
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
                                transport_drops_counter.clone(),
                                simulcast_layers,
                            )?);
                            info!("Costume video producer started for {} ({})", user_id, dir);
                        } else {
                            // Costume mode but no costume_dir -- fall back to EKG.
                            warn!(
                            "[{}] video_mode=costume but no costume_dir set, falling back to EKG",
                            user_id
                        );
                            let rms =
                                ekg_renderer::compute_rms_per_frame(&audio_data, 48000, ekg_fps);
                            let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
                            let renderer = EkgRenderer::new(ekg_color, ekg_width, ekg_height);
                            video_producer = Some(VideoProducer::from_ekg(
                                user_id.clone(),
                                renderer,
                                rms,
                                max_rms,
                                // rms was sampled at ekg_fps; the simulcast loop remaps the
                                // index to its (possibly higher) render fps (#1123 item 2).
                                ekg_fps,
                                packet_tx.clone(),
                                media_start,
                                loop_duration,
                                aq.clone(),
                                encoder_output_fps.clone(),
                                encoder_errors_generic.clone(),
                                encoder_frames_ok.clone(),
                                transport_drops_counter.clone(),
                                simulcast_layers,
                            )?);
                            info!("EKG video producer started for {} (fallback)", user_id);
                        }
                    } else {
                        let rms = ekg_renderer::compute_rms_per_frame(&audio_data, 48000, ekg_fps);
                        let max_rms = rms.iter().copied().fold(0.0f32, f32::max).max(0.01);
                        let renderer = EkgRenderer::new(ekg_color, ekg_width, ekg_height);
                        video_producer = Some(VideoProducer::from_ekg(
                            user_id.clone(),
                            renderer,
                            rms,
                            max_rms,
                            // rms was sampled at ekg_fps; the simulcast loop remaps the
                            // index to its (possibly higher) render fps (#1123 item 2).
                            ekg_fps,
                            packet_tx.clone(),
                            media_start,
                            loop_duration,
                            aq.clone(),
                            encoder_output_fps.clone(),
                            encoder_errors_generic.clone(),
                            encoder_frames_ok.clone(),
                            transport_drops_counter.clone(),
                            simulcast_layers,
                        )?);
                        info!("EKG video producer started for {}", user_id);
                    }
                }
            }

            info!("Client {} running", user_id);
            // The producers must outlive the hold: dropping one stops it.
            Ok::<_, anyhow::Error>((outbound_shim_task, audio_producer, video_producer))
        },
        &mut shutdown_rx,
        &mut closed,
        presence,
        &user_id,
    )
    .await;
    info!("Client {} stopped", user_id);
    let outbound_shim_task = started.and_then(|(task, audio_producer, video_producer)| {
        drop((audio_producer, video_producer));
        task
    });

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

    if result.is_ok() {
        info!("Client {} shut down cleanly", user_id);
    }
    result
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
