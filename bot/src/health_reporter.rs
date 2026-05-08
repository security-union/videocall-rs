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

//! Periodic HealthPacket sender for the synthetic bot.
//!
//! Builds a `HealthPacket` protobuf every second from accumulated `InboundStats`
//! counters, wraps it in a `PacketWrapper` with `packet_type = HEALTH`, and
//! sends it through the same packet channel used by audio/video producers.
//! This makes the bot visible to senders' adaptive quality feedback loops.

use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tokio::time;
use tracing::{debug, info, warn};

use crate::aq_controller::BotAq;
use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::health_packet::{
    HealthPacket as PbHealthPacket, NetEqNetwork as PbNetEqNetwork,
    NetEqOperationCounters as PbNetEqOpCounters, NetEqStats as PbNetEqStats,
    PeerStats as PbPeerStats, TierDwell as PbTierDwell, TierTransition as PbTierTransition,
    VideoStats as PbVideoStats,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Configuration for the health reporter.
pub struct HealthReporterConfig {
    pub client_config: ClientConfig,
    pub transport: Transport,
    /// Synthetic RTT to populate on every HealthPacket (ms). `None` leaves
    /// the field unset so passthrough bots look like real browsers whose
    /// WebRTC stats are absent. Set by main.rs to `2 × network_profile.latency_ms`
    /// when a netsim profile is active.
    pub simulated_rtt_ms: Option<f64>,
    /// Real measured RTT from RTT probes (f64 bits stored in AtomicU64).
    /// Used for passthrough bots that send actual RTT probes to the relay.
    /// Takes priority over `simulated_rtt_ms` when both are `None` for
    /// simulated but this field is set and non-zero.
    pub measured_rtt_ms: Option<Arc<AtomicU64>>,
    /// Shared counter incremented by the outbound shim/passthrough on every
    /// successful transport send. The health reporter reads + resets this
    /// every tick to derive packets_sent_per_sec.
    pub packets_sent_counter: Arc<AtomicU64>,
    /// Shared counter for transport-level drops (try_send failures on the
    /// outbound channel from any producer). Populated as
    /// `websocket_drops_total` or `datagram_drops_total` depending on
    /// transport type.
    pub transport_drops_counter: Arc<AtomicU64>,
    /// Current encoder output FPS written by the video producer. Reports the
    /// target framerate the encoder is configured at (bot always encodes at
    /// target — it does not drop frames).
    pub encoder_output_fps: Arc<AtomicU32>,
    /// Cumulative count of generic encoder errors (vpx encode failures).
    /// Incremented by the video producer on each failed encode call.
    pub encoder_errors_generic: Arc<AtomicU64>,
    /// Cumulative count of successfully encoded frames. Incremented by the
    /// video producer on each successful encode call.
    pub encoder_frames_ok: Arc<AtomicU64>,
    /// Shared counter for keyframe requests sent. Incremented by the
    /// `KeyframeRequester` each time it sends a request. Reports as
    /// `keyframe_requests_sent_total` in the HealthPacket.
    pub keyframe_requests_sent: Option<Arc<AtomicU64>>,
}

/// Spawn a health reporter task that sends HealthPacket protos every second.
///
/// The task runs until `quit` is set to true. It drains per-sender counters
/// from the shared `InboundStats`, computes per-second rates, and sends the
/// resulting HealthPacket through `packet_sender`.
pub fn spawn_health_reporter(
    config: HealthReporterConfig,
    stats: Arc<Mutex<InboundStats>>,
    packet_sender: Sender<OutboundFrame>,
    quit: Arc<AtomicBool>,
    aq: Arc<BotAq>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        // Skip the first immediate tick so the first report has a full second
        // of data.
        interval.tick().await;

        info!(
            "Health reporter started for {} in meeting {}",
            config.client_config.user_id, config.client_config.meeting_id
        );

        loop {
            interval.tick().await;

            if quit.load(Ordering::Relaxed) {
                break;
            }

            // Drain counters accumulated over the last ~1 second.
            let (sender_counters, total_packets) = {
                let mut s = stats.lock().unwrap();
                s.drain_health_counters()
            };

            // Read + reset the packets-sent counter to derive per-second rate.
            let packets_sent = config.packets_sent_counter.swap(0, Ordering::Relaxed);

            // Build HealthPacket proto.
            let packet_bytes = match build_health_packet(
                &config,
                &sender_counters,
                total_packets,
                packets_sent,
                &aq,
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(
                        "Failed to build health packet for {}: {}",
                        config.client_config.user_id, e
                    );
                    continue;
                }
            };

            let frame = OutboundFrame::new(MediaTypeLabel::Health, packet_bytes);
            if let Err(_e) = packet_sender.try_send(frame) {
                static HEALTH_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                let count = HEALTH_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                // Also increment the shared transport drops counter so the
                // cumulative total includes health packet drops.
                config
                    .transport_drops_counter
                    .fetch_add(1, Ordering::Relaxed);
                if count % 100 == 1 {
                    warn!(
                        "Dropped health packets due to full send channel (total: {})",
                        count,
                    );
                }
            } else {
                debug!(
                    "Sent health packet for {} ({} peers, {} total pkts)",
                    config.client_config.user_id,
                    sender_counters.len(),
                    total_packets,
                );
            }
        }

        info!(
            "Health reporter stopped for {}",
            config.client_config.user_id
        );
    });
}

/// Build a serialized `PacketWrapper` containing a `HealthPacket`.
fn build_health_packet(
    config: &HealthReporterConfig,
    sender_counters: &std::collections::HashMap<String, crate::inbound_stats::SenderHealthCounters>,
    total_packets: u64,
    packets_sent: u64,
    aq: &BotAq,
) -> anyhow::Result<Vec<u8>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64;

    let user_id = &config.client_config.user_id;

    let mut hp = PbHealthPacket::new();
    hp.session_id = user_id.clone();
    hp.meeting_id = config.client_config.meeting_id.clone();
    hp.reporting_user_id = user_id.as_bytes().to_vec();
    hp.timestamp_ms = now_ms;
    hp.reporting_audio_enabled = config.client_config.enable_audio;
    hp.reporting_video_enabled = config.client_config.enable_video;
    hp.display_name = Some(user_id.clone());

    // Connection info — active_server_url intentionally left empty because
    // HealthPackets are republished on NATS in cleartext and the URL contains
    // the JWT token. This matches the browser client behavior (see
    // videocall-client/src/health_reporter.rs:881).
    hp.active_server_type = match config.transport {
        Transport::WebTransport => "webtransport".to_string(),
        Transport::WebSocket => "websocket".to_string(),
    };
    // RTT: prefer simulated (netsim profile), then measured (RTT probe),
    // otherwise leave at default 0.0 (matching browser behavior when WebRTC
    // stats are unavailable).
    if let Some(rtt) = config.simulated_rtt_ms {
        hp.active_server_rtt_ms = rtt;
    } else if let Some(ref measured) = config.measured_rtt_ms {
        let bits = measured.load(Ordering::Relaxed);
        let rtt = f64::from_bits(bits);
        if rtt > 0.0 && rtt.is_finite() {
            hp.active_server_rtt_ms = rtt;
        }
    }

    // Tab state: bot is always active and never throttled
    hp.is_tab_visible = true;
    hp.is_tab_throttled = false;

    // Real current tier, driven by the adaptive-quality controller. This
    // used to be hard-coded to 0 which poisoned peer AQ decisions; now we
    // report the actual tier the bot is encoding at so senders see a truthful
    // signal.
    hp.adaptive_video_tier = Some(aq.video_tier_index());
    hp.adaptive_audio_tier = Some(aq.audio_tier_index());
    hp.screen_sharing_active = Some(false);

    // Encoder-decision telemetry, matching what the browser CameraEncoder
    // publishes (camera_encoder.rs: shared_encoder_*_bits). These fields
    // feed the Grafana AQ dashboards so bot-populated calls show the same
    // diagnostics as browser-populated ones.
    let fps_ratio = aq.last_fps_ratio();
    let worst_peer_fps = aq.last_worst_peer_fps();
    let bitrate_ratio = aq.last_bitrate_ratio();
    let target_bitrate = aq.last_target_bitrate_kbps();
    if fps_ratio.is_finite() && fps_ratio > 0.0 {
        hp.encoder_fps_ratio = Some(fps_ratio as f64);
    }
    if worst_peer_fps.is_finite() && worst_peer_fps > 0.0 {
        hp.encoder_worst_peer_fps = Some(worst_peer_fps as f64);
    }
    if bitrate_ratio.is_finite() && bitrate_ratio > 0.0 {
        hp.encoder_bitrate_ratio = Some(bitrate_ratio as f64);
    }
    if target_bitrate.is_finite() && target_bitrate > 0.0 {
        hp.encoder_target_bitrate_kbps = Some(target_bitrate as f64);
    }

    // Tier-transition events: drained once per heartbeat so the counter
    // `videocall_tier_transition_total` increments per event, matching the
    // browser's pattern in videocall-client/src/health_reporter.rs.
    for t in aq.drain_tier_transitions() {
        let mut pb_t = PbTierTransition::new();
        pb_t.direction = t.direction.to_string();
        pb_t.stream = t.stream.to_string();
        pb_t.from_tier = t.from_tier.clone();
        pb_t.to_tier = t.to_tier.clone();
        pb_t.trigger = t.trigger.to_string();
        hp.tier_transitions.push(pb_t);
    }

    // Overall inbound packet rate (all senders, all types)
    // The drain window is ~1 second, so count ~ rate.
    hp.packets_received_per_sec = Some(total_packets as f64);
    // Actual send rate derived from the shared counter that the outbound
    // shim/passthrough increments on every successful transport send.
    hp.packets_sent_per_sec = Some(packets_sent as f64);

    // Encoder output FPS — the target framerate the video encoder is
    // configured at (bot always encodes at target; it does not drop frames).
    let fps = config.encoder_output_fps.load(Ordering::Relaxed);
    if fps > 0 {
        hp.encoder_output_fps = Some(fps);
    }

    // Transport drop counters — cumulative count of try_send failures on the
    // outbound channel. Reported as websocket or datagram depending on the
    // active transport, matching the browser client's field semantics.
    let drops = config.transport_drops_counter.load(Ordering::Relaxed);
    if drops > 0 {
        match config.transport {
            Transport::WebSocket => {
                hp.websocket_drops_total = Some(drops);
            }
            Transport::WebTransport => {
                hp.datagram_drops_total = Some(drops);
            }
        }
    }

    // --- Fields 1 & 4: send_queue_bytes and keyframe_requests_sent_total ---
    // Bot has no meaningful send backpressure (single machine, channel → transport).
    hp.send_queue_bytes = Some(0);
    // Report actual keyframe requests sent if the requester is active,
    // otherwise report 0 to indicate the field is supported.
    let kf_sent = config
        .keyframe_requests_sent
        .as_ref()
        .map(|c| c.load(Ordering::Relaxed))
        .unwrap_or(0);
    hp.keyframe_requests_sent_total = Some(kf_sent);

    // --- Field 2: Climb-rate limiter telemetry ---
    let (
        crash_ceiling_active,
        crash_ceiling_tier_index,
        crash_ceiling_decay_ms,
        blocked_ceiling,
        blocked_slowdown,
        blocked_screen,
    ) = aq.snapshot_climb_limiter();
    hp.crash_ceiling_active = Some(crash_ceiling_active);
    if crash_ceiling_active {
        hp.crash_ceiling_tier_index = crash_ceiling_tier_index;
        hp.crash_ceiling_decay_ms = crash_ceiling_decay_ms;
    }
    if blocked_ceiling > 0 {
        hp.step_up_blocked_ceiling = Some(blocked_ceiling);
    }
    if blocked_slowdown > 0 {
        hp.step_up_blocked_slowdown = Some(blocked_slowdown);
    }
    if blocked_screen > 0 {
        hp.step_up_blocked_screen_share = Some(blocked_screen);
    }

    // Tier dwell samples: drained once per heartbeat so each sample appears
    // in exactly one HealthPacket, matching the browser's drain pattern.
    for (tier_label, dwell_ms) in aq.drain_dwell_samples() {
        let mut pb_d = PbTierDwell::new();
        pb_d.tier = tier_label.to_string();
        pb_d.dwell_ms = dwell_ms;
        hp.tier_dwells.push(pb_d);
    }

    // --- Field 3: Encoder error counters ---
    let errors_generic = config.encoder_errors_generic.load(Ordering::Relaxed);
    let frames_ok = config.encoder_frames_ok.load(Ordering::Relaxed);
    if errors_generic > 0 {
        hp.camera_encoder_errors_generic = Some(errors_generic);
    }
    if frames_ok > 0 {
        hp.camera_encoder_frames_submitted_ok = Some(frames_ok);
    }

    // Per-sender peer stats -- this is the critical part for AQ.
    // The drain window is ~1 second, so packet counts ~ per-second rates.
    for (sender_id, counters) in sender_counters {
        let mut ps = PbPeerStats::new();

        ps.can_listen = counters.audio_packets > 0;
        ps.can_see = counters.video_packets > 0;

        // Video stats -- fps_received is the field senders use to decide
        // quality tiers. Since we drain every ~1s, video_packets ~ fps.
        let mut vs = PbVideoStats::new();
        vs.fps_received = counters.video_packets as f64;
        vs.bitrate_kbps = counters.video_bytes * 8 / 1000; // bytes/s -> kbps
        vs.frames_decoded = counters.video_packets; // bot decodes every received frame
        ps.video_stats = ::protobuf::MessageField::some(vs);

        // NetEQ stats -- bot does not use NetEQ but populate realistic values.
        let mut ns = PbNetEqStats::new();
        ns.packets_per_sec = counters.audio_packets as f64;

        // Populate operation counters with normal_per_sec = audio packets
        let mut oc = PbNetEqOpCounters::new();
        oc.normal_per_sec = counters.audio_packets as f64;
        let mut network = PbNetEqNetwork::new();
        network.operation_counters = ::protobuf::MessageField::some(oc);
        ns.network = ::protobuf::MessageField::some(network);

        ps.neteq_stats = ::protobuf::MessageField::some(ns);

        // Audio concealment: bot has perfect playback (0% concealment)
        ps.audio_concealment_pct = 0.0;

        // Quality scores
        let audio_flowing = counters.audio_packets > 0;
        let video_fps = counters.video_packets as f64;
        let audio_score: f64 = if audio_flowing { 80.0 } else { 0.0 };
        let video_score: f64 = (video_fps / 30.0 * 100.0).min(100.0);
        let call_score: f64 = if audio_flowing {
            audio_score.min(video_score)
        } else {
            video_score
        };

        if audio_flowing {
            ps.audio_quality_score = Some(audio_score);
        }
        if counters.video_packets > 0 {
            ps.video_quality_score = Some(video_score);
        }
        if audio_flowing || counters.video_packets > 0 {
            ps.call_quality_score = Some(call_score);
        }

        hp.peer_stats.insert(sender_id.clone(), ps);
    }

    let hp_bytes = hp.write_to_bytes()?;

    let wrapper = PacketWrapper {
        packet_type: PacketType::HEALTH.into(),
        user_id: user_id.as_bytes().to_vec(),
        data: hp_bytes,
        ..Default::default()
    };

    Ok(wrapper.write_to_bytes()?)
}
