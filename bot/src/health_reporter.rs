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

    // Per-sender peer stats. The drain window is ~1 second, so packet counts
    // ~ per-second rates.
    for (sender_id, counters) in sender_counters {
        let mut ps = PbPeerStats::new();

        ps.can_listen = counters.audio_packets > 0;
        ps.can_see = peer_video_is_live(counters);

        // Video stats. Each inbound MediaPacket(VIDEO) is one encoded frame
        // (transport reassembles fragments), so video_packets over the ~1s drain
        // window ≈ frames/sec.
        //
        // `video_packets` counts only the rung this bot would DECODE (#2206) —
        // `InboundStats` filters to the highest arriving rung, mirroring the
        // browser's EXACT-MATCH guard, because the relay fans every rung to a
        // healthy receiver and an unfiltered count reads the ladder SUM.
        //
        // No longer fed to any sender AQ: #1108 Stage 2 removed receiver FPS from
        // the sender loop entirely, so this field is telemetry only.
        let mut vs = PbVideoStats::new();
        vs.fps_received = counters.video_packets as f64;
        vs.bitrate_kbps = counters.video_bytes * 8 / 1000; // bytes/s -> kbps
        vs.frames_decoded = counters.video_packets;
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
        // quality, above 5 — only near-frozen video is a defect. The browser also
        // subtracts loss/keyframe penalties the bot cannot compute; omitting those
        // strictly reduces divergence. Zero is OMITTED, not scored, matching the same
        // function's `fps <= 0.0 => None`; the full alert note lives there.
        //
        // ⚠ THE COST OF THAT PARITY, larger here than in the browser: with `--pin-layer
        // N`, a pinned rung that stops arriving holds fps at zero INDEFINITELY, not for
        // one availability window, so a starved selected stream never pulls the call
        // score down. It stays visible on `videocall_video_fps` (fed from `fps_received`
        // below) and `can_see`, but not on the alert — remedy tracked as #2249. Scoring
        // it low here would re-open the bot↔browser divergence this PR exists to close.
        let audio_quality = (counters.audio_packets > 0).then_some(80.0_f64);
        let video_fps = counters.video_packets as f64;
        let video_quality = (video_fps > 0.0).then(|| {
            if video_fps >= 5.0 {
                100.0
            } else {
                video_fps / 5.0 * 50.0
            }
        });
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

    let hp_bytes = hp.write_to_bytes()?;

    let wrapper = PacketWrapper {
        packet_type: PacketType::HEALTH.into(),
        user_id: user_id.as_bytes().to_vec(),
        data: hp_bytes,
        ..Default::default()
    };

    Ok(wrapper.write_to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::{build_health_packet, peer_video_is_live, HealthReporterConfig};
    use crate::aq_controller::BotAq;
    use crate::config::{ClientConfig, Transport};
    use crate::inbound_stats::SenderHealthCounters;
    use protobuf::Message;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;
    use videocall_aq::clock::{Clock, SystemClock};
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// The peer stats the REAL `build_health_packet` produces for one sender.
    fn peer_stats_for(counters: SenderHealthCounters) -> super::PbPeerStats {
        let config = HealthReporterConfig {
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
            encoder_output_fps: Arc::new(AtomicU32::new(0)),
            encoder_errors_generic: Arc::new(AtomicU64::new(0)),
            encoder_frames_ok: Arc::new(AtomicU64::new(0)),
            keyframe_requests_sent: None,
        };
        let aq = BotAq::new(Arc::new(SystemClock) as Arc<dyn Clock>);
        let mut senders = HashMap::new();
        senders.insert("alice".to_string(), counters);

        let bytes = build_health_packet(&config, &senders, 0, 0, &aq).expect("packet must build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper must parse");
        let hp = super::PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("health packet must parse");
        hp.peer_stats.get("alice").expect("alice present").clone()
    }

    /// The state this PR creates: the shed top rung is still inside the availability
    /// window, so the rung-filtered count is 0 while bytes keep arriving on base.
    const POST_SHED: SenderHealthCounters = SenderHealthCounters {
        audio_packets: 50,
        video_packets: 0,
        audio_bytes: 4000,
        video_bytes: 2000,
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
    fn a_shed_window_omits_the_video_score_instead_of_publishing_zero() {
        // The alert-bearing gauge. `video_packets == 0` with bytes flowing must NOT
        // become `Some(0.0)` on `videocall_call_quality_score` — it would satisfy
        // `MeetingQualityDegraded` (< 50, for 2m) for a healthy stream, and it would
        // diverge 80 points from the browser, which returns `None` here since #2190.
        let ps = peer_stats_for(POST_SHED);
        assert_eq!(
            ps.video_quality_score, None,
            "a window we cannot rate must be omitted, not scored 0"
        );
        assert_eq!(
            ps.call_quality_score,
            Some(80.0),
            "call score must fall through to audio alone, matching the browser"
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
        });
        assert_eq!(ps.video_quality_score, Some(20.0));
        assert_eq!(
            ps.call_quality_score,
            Some(20.0),
            "near-frozen video must pull the call score below the alert threshold"
        );
    }

    #[test]
    fn pinned_rung_starvation_is_deliberately_off_the_quality_alert() {
        // With `--pin-layer`, `video_packets == 0` while bytes flow is unbounded (the
        // pinned rung may never return), and the score is still omitted rather than
        // scored low. `fps_received` carries the starvation instead. Deliberate — see
        // #2249 before changing this.
        let ps = peer_stats_for(POST_SHED);
        assert_eq!(ps.video_quality_score, None, "omitted, not scored low");
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
        };
        assert!(
            peer_video_is_live(&shed_window),
            "liveness must follow arrivals; reading the rung-filtered count blanks \
             can_see for the whole availability window after a ladder shed"
        );

        // Genuinely nothing arriving.
        assert!(!peer_video_is_live(&SenderHealthCounters::default()));
    }
}
