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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time;
use tracing::{debug, info, warn};

use videocall_aq::constants::AUDIO_ACTIVE_PPS_GATE;

/// Drain cadence: every published rate is a count over one of these windows.
const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Shortest window that can carry a rate — half the drain cadence.
const MIN_DRAIN_WINDOW_MS: f64 = DRAIN_INTERVAL.as_millis() as f64 / 2.0;

use crate::aq_controller::BotAq;
use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
use crate::transport::{
    MediaTypeLabel, OutboundFrame, OutboundFrameSender, WebSocketStreamByteCounters,
    WebSocketStreamByteSnapshot,
};
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
    /// Set by main.rs on WebSocket runs only; nothing bills these counters on
    /// another transport, so the WebSocket-only `ws_offered_bytes_*` fields stay
    /// absent there.
    pub websocket_stream_bytes: Option<Arc<WebSocketStreamByteCounters>>,
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
    packet_sender: OutboundFrameSender,
    quit: Arc<AtomicBool>,
    aq: Arc<BotAq>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(DRAIN_INTERVAL);
        // `Burst`, the default, fires missed ticks back to back — sliver windows.
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        // Skip the first immediate tick so the first report has a full second
        // of data.
        interval.tick().await;
        let mut window_start = Instant::now();

        info!(
            "Health reporter started for {} in meeting {}",
            config.client_config.user_id, config.client_config.meeting_id
        );

        loop {
            interval.tick().await;

            if quit.load(Ordering::Relaxed) {
                break;
            }

            // Skip WITHOUT draining: the counters roll into the next window.
            let window_ms = window_start.elapsed().as_secs_f64() * 1000.0;
            if window_ms < MIN_DRAIN_WINDOW_MS {
                continue;
            }
            window_start = Instant::now();

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
                window_ms,
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

/// Whether a peer's video is LIVE, for `can_see`.
///
/// Reads bytes, not `video_packets`: the latter is rung-filtered (#2206) and sits at
/// zero for the whole availability window after a ladder shed, so it would report
/// `can_see = false` while frames are still arriving on the base rung. The browser
/// drives `can_see` off a clock that advances on every video event regardless of
/// fps, so it reports `fps_received = 0` with `can_see = true`.
pub(crate) fn peer_video_is_live(counters: &crate::inbound_stats::SenderHealthCounters) -> bool {
    counters.video_bytes > 0
}

/// Audio counterpart of [`peer_video_is_live`], on bytes for the same reason.
pub(crate) fn peer_audio_is_live(counters: &crate::inbound_stats::SenderHealthCounters) -> bool {
    counters.audio_bytes > 0
}

/// Whether the DECODED audio is worth scoring; bytes would score 80 while nothing does.
pub(crate) fn peer_audio_is_scorable(audio_packets_per_sec: f64) -> bool {
    audio_packets_per_sec >= AUDIO_ACTIVE_PPS_GATE
}

/// Build a serialized `PacketWrapper` containing a `HealthPacket`.
fn build_health_packet(
    config: &HealthReporterConfig,
    sender_counters: &std::collections::HashMap<String, crate::inbound_stats::SenderHealthCounters>,
    total_packets: u64,
    packets_sent: u64,
    aq: &BotAq,
    window_ms: f64,
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
    // diagnostics as browser-populated ones. NOTE(#1184): the dead
    // encoder_fps_ratio / encoder_bitrate_ratio proto fields were removed; only
    // the live p75 (encoder-queue depth) + target-bitrate signals remain.
    let p75_peer_fps = aq.last_p75_peer_fps();
    let target_bitrate = aq.last_target_bitrate_kbps();
    if p75_peer_fps.is_finite() && p75_peer_fps > 0.0 {
        hp.encoder_p75_peer_fps = Some(p75_peer_fps as f64);
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

    // Divides by the MEASURED window, not a fixed second.
    let per_sec = |count: u64| count as f64 * 1000.0 / window_ms.max(MIN_DRAIN_WINDOW_MS);
    for (sender_id, counters) in sender_counters {
        let mut ps = PbPeerStats::new();
        let audio_pps = per_sec(counters.audio_packets);
        let video_fps = per_sec(counters.video_packets);

        ps.can_listen = peer_audio_is_live(counters);
        ps.can_see = peer_video_is_live(counters);

        // Video stats. Each inbound MediaPacket(VIDEO) is one encoded frame
        //
        // `video_packets` counts only the rung this bot would DECODE (#2206) —
        // `InboundStats` filters to the highest arriving rung, mirroring the
        // browser's EXACT-MATCH guard, because the relay fans every rung to a
        // healthy receiver and an unfiltered count reads the ladder SUM.
        //
        // No longer fed to any sender AQ: #1108 Stage 2 removed receiver FPS from
        // the sender loop entirely, so this field is telemetry only.
        let vs_bitrate_kbps = counters.video_bytes * 8 / 1000; // bytes/s -> kbps
        let mut vs = PbVideoStats::new();
        vs.fps_received = video_fps;
        vs.bitrate_kbps = vs_bitrate_kbps;
        vs.frames_decoded = counters.video_packets;
        ps.video_stats = ::protobuf::MessageField::some(vs);

        // NetEQ stats -- bot does not use NetEQ but populate realistic values.
        let mut ns = PbNetEqStats::new();
        ns.packets_per_sec = audio_pps;

        // Populate operation counters with normal_per_sec = audio packets
        let mut oc = PbNetEqOpCounters::new();
        oc.normal_per_sec = audio_pps;
        let mut network = PbNetEqNetwork::new();
        network.operation_counters = ::protobuf::MessageField::some(oc);
        ns.network = ::protobuf::MessageField::some(network);

        ps.neteq_stats = ::protobuf::MessageField::some(ns);

        // Audio concealment: bot has perfect playback (0% concealment)
        ps.audio_concealment_pct = 0.0;

        // #2424. Folded UNCONDITIONALLY: the metrics server sets these with
        // `if let Some(x)`, and its #1092 prune fires only for a peer ABSENT from the
        // packet — so a PRESENT peer with the field omitted leaves the gauge holding its
        // previous reading. Bounded by the rung availability window (see
        // `SenderHealthCounters`), so 0 means "no in-window discontinuity", not "no freeze".
        ps.video_seq_loss_per_sec = Some(per_sec(counters.video_seq_gaps));
        ps.audio_datagram_loss_per_sec = Some(match config.transport {
            Transport::WebTransport => per_sec(counters.audio_seq_gaps),
            Transport::WebSocket => 0.0,
        });

        // Quality scores.
        //
        // ⚠ #2206 CHANGED THIS TERM'S NUMERATOR AND IT FEEDS AN ALERT. `video_packets` is
        // now the DECODED rung, not the ladder sum, so the old `fps / 30 * 100` curve —
        // calibrated when a 3-rung ladder summed to ~52 and pinned this at 100 — reads
        // 23.3 for a healthy rung-0 receiver (7 fps) and 50.0 for rung 1 (15 fps). That
        // publishes < 50 onto `videocall_call_quality_score` → `MeetingQualityDegraded`
        // (`avg by (meeting_id)(...) < 50`, for: 2m, no bot exclusion), i.e. a
        // `--pin-layer 0` fleet run would alert continuously on a healthy meeting.
        //
        // So use the browser's SATURATING curve verbatim (#2190,
        // `videocall-client/src/health_reporter.rs`): fps is hardware/rung context, not
        // quality, above 5 — only near-frozen video is a defect. Zero is OMITTED.
        //
        // #2249: `video_bytes` is UNFILTERED while `video_packets` is the decoded rung, so
        // fps 0 with live bitrate is the browser's receiving-not-decoding signature.
        let audio_quality = peer_audio_is_scorable(audio_pps).then_some(80.0_f64);
        let video_quality = if video_fps > 0.0 {
            Some(if video_fps >= 5.0 {
                100.0
            } else {
                video_fps / 5.0 * 50.0
            })
        } else if vs_bitrate_kbps > 0 {
            Some(0.0)
        } else {
            None
        };
        // Worst of whichever streams are active — same match arms as the browser's.
        let call_score = match (audio_quality, video_quality) {
            (Some(a), Some(v)) => Some(a.min(v)),
            (Some(a), None) => Some(a),
            (None, Some(v)) => Some(v),
            (None, None) => None,
        };

        if let Some(a) = audio_quality {
            ps.audio_quality_score = Some(a);
        }
        if let Some(v) = video_quality {
            ps.video_quality_score = Some(v);
        }
        if let Some(c) = call_score {
            ps.call_quality_score = Some(c);
        }

        hp.peer_stats.insert(sender_id.clone(), ps);
    }

    if let Some(counters) = &config.websocket_stream_bytes {
        set_ws_stream_bytes(&mut hp, counters.snapshot());
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

/// Sets `ws_offered_bytes_*` only. `ws_dropped_bytes_*` means "discarded by the
/// browser's 1 MiB `bufferedAmount` guard"; the bot's tungstenite send path has
/// no such discard gate, so it leaves those fields unset (issue 2520).
fn set_ws_stream_bytes(hp: &mut PbHealthPacket, bytes: WebSocketStreamByteSnapshot) {
    let nonzero = |v: u64| (v != 0).then_some(v);
    hp.ws_offered_bytes_audio = nonzero(bytes.offered_audio);
    hp.ws_offered_bytes_video = nonzero(bytes.offered_video);
    hp.ws_offered_bytes_control = nonzero(bytes.offered_control);
}

#[cfg(test)]
mod tests {
    use super::{
        build_health_packet, peer_audio_is_live, peer_audio_is_scorable, peer_video_is_live,
        HealthReporterConfig,
    };
    use crate::aq_controller::BotAq;
    use crate::config::{ClientConfig, Transport};
    use crate::inbound_stats::SenderHealthCounters;
    use crate::transport::{MediaTypeLabel, WebSocketStreamByteCounters};
    use protobuf::Message;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;
    use videocall_aq::clock::{Clock, SystemClock};
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// Peer stats from the REAL `build_health_packet`, over a nominal 1s window.
    fn peer_stats_for(counters: SenderHealthCounters) -> super::PbPeerStats {
        peer_stats_for_window(counters, 1000.0)
    }

    fn peer_stats_for_window(counters: SenderHealthCounters, window_ms: f64) -> super::PbPeerStats {
        peer_stats_on(counters, window_ms, Transport::WebSocket)
    }

    fn peer_stats_on(
        counters: SenderHealthCounters,
        window_ms: f64,
        transport: Transport,
    ) -> super::PbPeerStats {
        let config = HealthReporterConfig {
            client_config: ClientConfig {
                user_id: "bot".to_string(),
                meeting_id: "room".to_string(),
                enable_audio: true,
                enable_video: true,
            },
            transport,
            simulated_rtt_ms: None,
            measured_rtt_ms: None,
            packets_sent_counter: Arc::new(AtomicU64::new(0)),
            transport_drops_counter: Arc::new(AtomicU64::new(0)),
            websocket_stream_bytes: None,
            encoder_output_fps: Arc::new(AtomicU32::new(0)),
            encoder_errors_generic: Arc::new(AtomicU64::new(0)),
            encoder_frames_ok: Arc::new(AtomicU64::new(0)),
            keyframe_requests_sent: None,
        };
        let aq = BotAq::new(Arc::new(SystemClock) as Arc<dyn Clock>);
        let mut senders = HashMap::new();
        senders.insert("alice".to_string(), counters);

        let bytes = build_health_packet(&config, &senders, 0, 0, &aq, window_ms)
            .expect("packet must build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper must parse");
        let hp = super::PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("health packet must parse");
        hp.peer_stats.get("alice").expect("alice present").clone()
    }

    fn health_config_with_ws_counters(
        counters: Arc<WebSocketStreamByteCounters>,
    ) -> HealthReporterConfig {
        HealthReporterConfig {
            client_config: ClientConfig {
                user_id: "bot".to_string(),
                meeting_id: "room".to_string(),
                enable_audio: true,
                enable_video: true,
            },
            transport: Transport::WebSocket,
            simulated_rtt_ms: None,
            measured_rtt_ms: None,
            packets_sent_counter: Arc::new(AtomicU64::new(0)),
            transport_drops_counter: Arc::new(AtomicU64::new(0)),
            websocket_stream_bytes: Some(counters),
            encoder_output_fps: Arc::new(AtomicU32::new(0)),
            encoder_errors_generic: Arc::new(AtomicU64::new(0)),
            encoder_frames_ok: Arc::new(AtomicU64::new(0)),
            keyframe_requests_sent: None,
        }
    }

    fn health_packet_for_config(config: &HealthReporterConfig) -> super::PbHealthPacket {
        let aq = BotAq::new(Arc::new(SystemClock) as Arc<dyn Clock>);
        let bytes = build_health_packet(config, &HashMap::new(), 0, 0, &aq, 1000.0)
            .expect("packet must build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper must parse");
        super::PbHealthPacket::parse_from_bytes(&wrapper.data).expect("health packet must parse")
    }

    #[test]
    fn websocket_stream_byte_fields_leave_zero_unset() {
        let counters = Arc::new(WebSocketStreamByteCounters::default());
        let config = health_config_with_ws_counters(counters);
        let hp = health_packet_for_config(&config);

        assert_eq!(hp.ws_offered_bytes_audio, None);
        assert_eq!(hp.ws_offered_bytes_video, None);
        assert_eq!(hp.ws_offered_bytes_screen, None);
        assert_eq!(hp.ws_offered_bytes_control, None);
        assert_eq!(hp.ws_dropped_bytes_audio, None);
        assert_eq!(hp.ws_dropped_bytes_video, None);
        assert_eq!(hp.ws_dropped_bytes_screen, None);
        assert_eq!(hp.ws_dropped_bytes_control, None);
    }

    #[test]
    fn websocket_stream_byte_fields_publish_nonzero_buckets() {
        let counters = Arc::new(WebSocketStreamByteCounters::default());
        counters.record_offered(MediaTypeLabel::Audio, 11);
        counters.record_offered(MediaTypeLabel::Video, 22);
        counters.record_offered(MediaTypeLabel::Other, 44);

        let config = health_config_with_ws_counters(counters);
        let hp = health_packet_for_config(&config);

        assert_eq!(hp.ws_offered_bytes_audio, Some(11));
        assert_eq!(hp.ws_offered_bytes_video, Some(22));
        assert_eq!(hp.ws_offered_bytes_screen, None);
        assert_eq!(hp.ws_offered_bytes_control, Some(44));
    }

    /// The bot has no `bufferedAmount`-equivalent discard gate, so the drop
    /// fields must stay absent even on a run that offered bytes on every bucket.
    #[test]
    fn websocket_dropped_byte_fields_are_never_published() {
        let counters = Arc::new(WebSocketStreamByteCounters::default());
        for kind in MediaTypeLabel::ALL {
            counters.record_offered(kind, 100);
        }

        let config = health_config_with_ws_counters(counters);
        let hp = health_packet_for_config(&config);

        assert!(hp.ws_offered_bytes_audio.is_some());
        assert!(hp.ws_offered_bytes_video.is_some());
        assert!(hp.ws_offered_bytes_control.is_some());
        assert_eq!(hp.ws_dropped_bytes_audio, None);
        assert_eq!(hp.ws_dropped_bytes_video, None);
        assert_eq!(hp.ws_dropped_bytes_screen, None);
        assert_eq!(hp.ws_dropped_bytes_control, None);
    }

    /// The state this PR creates: the shed top rung is still inside the availability
    /// window, so the rung-filtered count is 0 while bytes keep arriving on base.
    const POST_SHED: SenderHealthCounters = SenderHealthCounters {
        audio_packets: 50,
        video_packets: 0,
        audio_bytes: 4000,
        video_bytes: 2000,
        audio_seq_gaps: 0,
        video_seq_gaps: 0,
    };

    #[test]
    fn the_emitted_packet_keeps_can_see_true_through_a_ladder_shed() {
        let ps = peer_stats_for(POST_SHED);
        assert!(
            ps.can_see,
            "can_see must follow arrivals; the rung-filtered count blanks it for the \
             whole availability window after a shed"
        );
    }

    #[test]
    fn a_receiving_but_not_decoding_window_scores_zero() {
        let ps = peer_stats_for(POST_SHED);
        assert_eq!(ps.video_quality_score, Some(0.0));
        assert_eq!(
            ps.call_quality_score,
            Some(0.0),
            "the call score must take the stalled video, not fall through to audio"
        );
    }

    #[test]
    fn a_window_with_no_video_bytes_at_all_omits_the_score() {
        let ps = peer_stats_for(SenderHealthCounters {
            audio_packets: 50,
            video_packets: 0,
            audio_bytes: 4000,
            video_bytes: 0,
            ..Default::default()
        });
        assert_eq!(ps.video_quality_score, None);
        assert_eq!(
            ps.call_quality_score,
            Some(80.0),
            "with no video signal the call score is the audio score"
        );
    }

    #[test]
    fn a_healthy_low_rung_receiver_does_not_trip_the_quality_alert() {
        // A healthy receiver on a low rung must not read as degraded: `video_packets` is
        // the decoded rung, so a linear `fps / 30 * 100` scores rung 0 at 23.3 and rung 1
        // at exactly 50.0 — at or under `MeetingQualityDegraded`'s `< 50`.
        //
        // The real ladder is 7 / 15 / 30 fps (videocall-aq SIMULCAST_VIDEO_LAYERS).
        for (fps, rung) in [(7u64, "base"), (15, "middle"), (30, "top")] {
            let ps = peer_stats_for(SenderHealthCounters {
                audio_packets: 50,
                video_packets: fps,
                audio_bytes: 4000,
                video_bytes: fps * 2000,
                ..Default::default()
            });
            assert_eq!(
                ps.video_quality_score,
                Some(100.0),
                "decoding the {rung} rung at {fps}fps is healthy, not degraded"
            );
            let call = ps.call_quality_score.expect("call score present");
            assert!(
                call >= 50.0,
                "the {} rung must not trip MeetingQualityDegraded (< 50); got {}",
                rung,
                call
            );
        }
    }

    #[test]
    fn near_frozen_video_still_scores_low() {
        // The guard must not flatten everything to 100: 1-4 fps is the near-frozen band
        // the browser's curve deliberately scores 10-40, and it SHOULD pull the alert.
        let ps = peer_stats_for(SenderHealthCounters {
            audio_packets: 50,
            video_packets: 2,
            audio_bytes: 4000,
            video_bytes: 4000,
            ..Default::default()
        });
        assert_eq!(ps.video_quality_score, Some(20.0));
        assert_eq!(
            ps.call_quality_score,
            Some(20.0),
            "near-frozen video must pull the call score below the alert threshold"
        );
    }

    #[test]
    fn pinned_rung_starvation_reaches_the_quality_alert() {
        // Unbounded under `--pin-layer`, so unlike the shed window above this one holds
        // `MeetingQualityDegraded`'s `for: 2m` (#2249).
        let ps = peer_stats_for(POST_SHED);
        assert_eq!(ps.video_quality_score, Some(0.0));
        assert_eq!(
            ps.video_stats.fps_received, 0.0,
            "the starvation must remain observable on the fps gauge's source field"
        );
        assert!(ps.can_see, "and the peer is still visibly sending bytes");
    }

    #[test]
    fn a_decoding_window_still_scores_video() {
        // The guard must not swallow healthy windows: 30 fps decoded clamps to 100,
        // and the call score is the worse of the two active streams (audio 80).
        let ps = peer_stats_for(SenderHealthCounters {
            audio_packets: 50,
            video_packets: 30,
            audio_bytes: 4000,
            video_bytes: 60_000,
            ..Default::default()
        });
        assert_eq!(ps.video_quality_score, Some(100.0));
        assert_eq!(ps.call_quality_score, Some(80.0));
    }

    #[test]
    fn a_video_only_peer_with_no_audio_still_scores_the_call() {
        // `(None, Some(v))` arm — without it a video-only peer would publish no call
        // score at all.
        let ps = peer_stats_for(SenderHealthCounters {
            audio_packets: 0,
            video_packets: 15,
            audio_bytes: 0,
            video_bytes: 30_000,
            ..Default::default()
        });
        assert_eq!(ps.audio_quality_score, None);
        assert_eq!(ps.video_quality_score, Some(100.0));
        assert_eq!(ps.call_quality_score, Some(100.0));
    }

    #[test]
    fn video_liveness_reads_arrival_not_the_rung_filtered_count() {
        // The post-shed window: bytes still arriving on the base rung, but
        // `video_packets` is zero because the shed top rung is still inside the
        // availability window. `can_see` must stay TRUE — a false negative here is
        // exported as `videocall_peer_can_see` and panelled in Grafana, so it would
        // read as "peer cannot see" while video flows.
        let shed_window = SenderHealthCounters {
            audio_packets: 0,
            video_packets: 0,
            audio_bytes: 0,
            video_bytes: 2000,
            ..Default::default()
        };
        assert!(
            peer_video_is_live(&shed_window),
            "liveness must follow arrivals; reading the rung-filtered count blanks \
             can_see for the whole availability window after a ladder shed"
        );

        // Genuinely nothing arriving.
        assert!(!peer_video_is_live(&SenderHealthCounters::default()));
    }

    #[test]
    fn audio_liveness_and_audio_scoring_split_across_a_rung_shed() {
        let shed_window = SenderHealthCounters {
            audio_packets: 0,
            video_packets: 0,
            audio_bytes: 5000,
            video_bytes: 0,
            ..Default::default()
        };
        assert!(peer_audio_is_live(&shed_window));
        assert!(!peer_audio_is_scorable(0.0));
        assert!(!peer_audio_is_live(&SenderHealthCounters::default()));

        let ps = peer_stats_for(shed_window);
        assert!(
            ps.can_listen,
            "liveness must follow arrivals or a shed blanks it for a whole window"
        );
        assert_eq!(
            ps.audio_quality_score, None,
            "scoring bytes the receiver cannot decode publishes 80 for starvation"
        );
        assert_eq!(
            ps.call_quality_score, None,
            "no scorable stream means no call score, not a healthy one"
        );
        assert_eq!(
            ps.neteq_stats.packets_per_sec, 0.0,
            "the honest starvation signal stays on the decoded count"
        );
    }

    #[test]
    fn the_audio_score_threshold_matches_the_browser_rate_gate() {
        assert!(!peer_audio_is_scorable(1.999));
        assert!(peer_audio_is_scorable(2.0));
        assert!(!peer_audio_is_scorable(0.0));
    }

    #[test]
    fn rates_divide_by_the_measured_window_not_a_fixed_second() {
        let ps = peer_stats_for_window(
            SenderHealthCounters {
                audio_packets: 5,
                video_packets: 5,
                audio_bytes: 500,
                video_bytes: 500,
                ..Default::default()
            },
            500.0,
        );
        assert_eq!(ps.neteq_stats.packets_per_sec, 10.0);
        assert_eq!(
            ps.neteq_stats.network.operation_counters.normal_per_sec,
            10.0
        );
        assert_eq!(ps.video_stats.fps_received, 10.0);
    }

    #[test]
    fn a_sliver_window_cannot_explode_the_published_rates() {
        // `drain_health_counters` is a `mem::take`, so a microsecond window can still
        // carry a whole post-stall backlog.
        let ps = peer_stats_for_window(
            SenderHealthCounters {
                audio_packets: 100,
                video_packets: 100,
                audio_bytes: 10_000,
                video_bytes: 10_000,
                ..Default::default()
            },
            0.05,
        );
        // A plausibility ceiling, not the formula: audio is 50 pkt/s, video 30 fps.
        const CEILING: f64 = 1000.0;
        assert!(
            ps.neteq_stats.packets_per_sec < CEILING,
            "packets_per_sec exploded: {}",
            ps.neteq_stats.packets_per_sec
        );
        assert!(
            ps.neteq_stats.network.operation_counters.normal_per_sec < CEILING,
            "normal_per_sec exploded: {}",
            ps.neteq_stats.network.operation_counters.normal_per_sec
        );
        assert!(
            ps.video_stats.fps_received < CEILING,
            "fps_received exploded: {}",
            ps.video_stats.fps_received
        );
    }
    /// A lossy receive window: 12 video and 9 audio positions skipped.
    const LOSSY: SenderHealthCounters = SenderHealthCounters {
        audio_packets: 50,
        video_packets: 30,
        audio_bytes: 4000,
        video_bytes: 60_000,
        audio_seq_gaps: 9,
        video_seq_gaps: 12,
    };

    #[test]
    fn peer_stats_publish_the_measured_video_loss_rate() {
        let ps = peer_stats_for(LOSSY);
        assert_eq!(
            ps.video_seq_loss_per_sec,
            Some(12.0),
            "field 15 must carry the sender's own windowed gap rate"
        );
    }

    #[test]
    fn a_clean_window_publishes_zero_loss_rather_than_omitting_it() {
        // The chosen semantic: for a peer PRESENT in the packet, an omitted field does
        // not read as "unknown" downstream — it holds the gauge at its last value.
        let ps = peer_stats_for(SenderHealthCounters {
            audio_packets: 50,
            video_packets: 30,
            audio_bytes: 4000,
            video_bytes: 60_000,
            ..Default::default()
        });
        assert_eq!(
            ps.video_seq_loss_per_sec,
            Some(0.0),
            "a clean window must publish 0.0, never absence"
        );
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(0.0));
    }

    #[test]
    fn a_peer_that_sent_nothing_still_publishes_both_loss_fields() {
        let ps = peer_stats_for(SenderHealthCounters::default());
        assert_eq!(ps.video_seq_loss_per_sec, Some(0.0));
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(0.0));
    }

    #[test]
    fn audio_datagram_loss_is_definitionally_zero_on_websocket() {
        let ps = peer_stats_on(LOSSY, 1000.0, Transport::WebSocket);
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(0.0));
        assert_eq!(
            ps.video_seq_loss_per_sec,
            Some(12.0),
            "the video field has no transport gate"
        );
    }

    #[test]
    fn audio_datagram_loss_publishes_the_measured_rate_on_webtransport() {
        let ps = peer_stats_on(LOSSY, 1000.0, Transport::WebTransport);
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(9.0));
    }

    #[test]
    fn loss_rates_divide_by_the_measured_window_not_a_fixed_second() {
        let ps = peer_stats_on(LOSSY, 2000.0, Transport::WebTransport);
        assert_eq!(ps.video_seq_loss_per_sec, Some(6.0));
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(4.5));
    }

    #[test]
    fn a_sliver_window_cannot_explode_the_published_loss_rates() {
        let ps = peer_stats_on(LOSSY, 1.0, Transport::WebTransport);
        assert_eq!(ps.video_seq_loss_per_sec, Some(24.0));
        assert_eq!(ps.audio_datagram_loss_per_sec, Some(18.0));
    }
}
