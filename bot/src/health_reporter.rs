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
//! Sends a session-level `HealthPacket` every `interval` (the browser's 5 s by
//! default) through the same packet channel the media producers use, so the
//! relay and metrics-api carry one HEALTH stream per session as they do for a
//! browser. The packet holds only values the bot measures: identity, transport,
//! probe RTT, packet rates, drops, keyframe requests, AQ and encoder telemetry.
//! It carries no `peer_stats` and no quality scores.

use protobuf::Message;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time;
use tracing::{debug, info, warn};

use crate::aq_controller::BotAq;
use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
use crate::transport::{
    MediaTypeLabel, OutboundFrame, OutboundFrameSender, WebSocketStreamByteCounters,
    WebSocketStreamByteSnapshot,
};
use videocall_types::protos::health_packet::{
    HealthPacket as PbHealthPacket, TierDwell as PbTierDwell, TierTransition as PbTierTransition,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Configuration for the health reporter.
pub struct HealthReporterConfig {
    pub client_config: ClientConfig,
    /// HEALTH cadence; every published rate is a count over one of these windows.
    pub interval: Duration,
    pub transport: Transport,
    /// Measured RTT from RTT probes (f64 bits stored in AtomicU64); `None` or
    /// zero leaves the field unset.
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

/// Spawn a health reporter task that sends HealthPacket protos every `config.interval`.
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
        let mut interval = time::interval(config.interval);
        // Shortest window that can carry a rate: half the cadence.
        let min_window_ms = config.interval.as_secs_f64() * 1000.0 / 2.0;
        // `Burst`, the default, fires missed ticks back to back — sliver windows.
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        // Skip the first immediate tick so the first report has a full second
        // of data.
        interval.tick().await;
        let mut window_start = Instant::now();
        // The browser's value until SESSION_ASSIGNED arrives.
        let placeholder_session = format!(
            "session_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );

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
            if window_ms < min_window_ms {
                continue;
            }
            window_start = Instant::now();

            let (total_packets, session_id) = {
                let mut s = stats.lock().unwrap();
                (s.take_health_total(), s.own_session_id())
            };
            let session_id = health_session_id(session_id, &placeholder_session);

            // Read + reset the packets-sent counter to derive per-second rate.
            let packets_sent = config.packets_sent_counter.swap(0, Ordering::Relaxed);

            // Build HealthPacket proto.
            let packet_bytes = match build_health_packet(
                &config,
                &session_id,
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
                    "Sent health packet for {} ({} total pkts)",
                    config.client_config.user_id, total_packets,
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
/// `HealthPacket.session_id`: the relay session from `SESSION_ASSIGNED`, as the
/// browser sends (`videocall-client` `set_session_id`), else `placeholder`.
pub(crate) fn health_session_id(assigned: Option<u64>, placeholder: &str) -> String {
    assigned.map_or_else(|| placeholder.to_string(), |id| id.to_string())
}

fn build_health_packet(
    config: &HealthReporterConfig,
    session_id: &str,
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
    hp.session_id = session_id.to_string();
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
    // RTT from the probe only; a value derived from the netsim profile would be synthetic.
    if let Some(ref measured) = config.measured_rtt_ms {
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

    // Overall inbound and outbound packet rates over the measured window.
    // The reporter skips windows under half the cadence, so that is the floor.
    let min_window_ms = config.interval.as_secs_f64() * 1000.0 / 2.0;
    let window_rate = |count: u64| count as f64 * 1000.0 / window_ms.max(min_window_ms);
    hp.packets_received_per_sec = Some(window_rate(total_packets));
    hp.packets_sent_per_sec = Some(window_rate(packets_sent));

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

    // send_queue_bytes stays unset: the bot does not measure its send queue.
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
    use super::{build_health_packet, HealthReporterConfig};
    use crate::aq_controller::BotAq;
    use crate::config::{ClientConfig, Transport};
    use crate::transport::{MediaTypeLabel, WebSocketStreamByteCounters};
    use protobuf::Message;
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;
    use videocall_aq::clock::{Clock, SystemClock};
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    fn health_config(counters: Option<Arc<WebSocketStreamByteCounters>>) -> HealthReporterConfig {
        HealthReporterConfig {
            client_config: ClientConfig {
                user_id: "bot".to_string(),
                meeting_id: "room".to_string(),
                enable_audio: true,
                enable_video: true,
                heartbeat_interval: std::time::Duration::from_secs(5),
            },
            interval: std::time::Duration::from_secs(5),
            transport: Transport::WebSocket,
            measured_rtt_ms: None,
            packets_sent_counter: Arc::new(AtomicU64::new(0)),
            transport_drops_counter: Arc::new(AtomicU64::new(0)),
            websocket_stream_bytes: counters,
            encoder_output_fps: Arc::new(AtomicU32::new(0)),
            encoder_errors_generic: Arc::new(AtomicU64::new(0)),
            encoder_frames_ok: Arc::new(AtomicU64::new(0)),
            keyframe_requests_sent: None,
        }
    }

    fn health_config_with_ws_counters(
        counters: Arc<WebSocketStreamByteCounters>,
    ) -> HealthReporterConfig {
        health_config(Some(counters))
    }

    fn packet(
        config: &HealthReporterConfig,
        received: u64,
        sent: u64,
        window_ms: f64,
    ) -> super::PbHealthPacket {
        let aq = BotAq::new(Arc::new(SystemClock) as Arc<dyn Clock>);
        let bytes = build_health_packet(config, "42", received, sent, &aq, window_ms)
            .expect("packet must build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper must parse");
        super::PbHealthPacket::parse_from_bytes(&wrapper.data).expect("health packet must parse")
    }

    fn health_packet_for_config(config: &HealthReporterConfig) -> super::PbHealthPacket {
        packet(config, 0, 0, 1000.0)
    }

    #[test]
    fn health_carries_no_per_pair_stats_or_synthetic_values() {
        let hp = packet(&health_config(None), 600, 300, 5000.0);
        assert!(
            hp.peer_stats.is_empty(),
            "no per-pair HEALTH from Rust bots"
        );
        assert_eq!(hp.send_queue_bytes, None, "the bot does not measure it");
        assert_eq!(
            hp.active_server_rtt_ms, 0.0,
            "no RTT without a probe sample"
        );
        assert_eq!(hp.session_id, "42", "the relay session, not the user id");
        assert_eq!(hp.active_server_type, "websocket");
    }

    #[test]
    fn health_session_id_is_the_assigned_relay_session() {
        use crate::inbound_stats::{test_packets, InboundStats};
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        let mut stats = InboundStats::default();
        assert_eq!(
            super::health_session_id(stats.own_session_id(), "session_9"),
            "session_9"
        );
        stats.record_packet(
            "me",
            &test_packets::control(PacketType::SESSION_ASSIGNED, 42, vec![]),
        );
        assert_eq!(
            super::health_session_id(stats.own_session_id(), "session_9"),
            "42"
        );
    }

    #[test]
    fn a_short_health_interval_divides_by_its_own_window() {
        let mut config = health_config(None);
        config.interval = std::time::Duration::from_millis(250);
        let hp = packet(&config, 100, 50, 250.0);
        assert_eq!(hp.packets_received_per_sec, Some(400.0));
        assert_eq!(hp.packets_sent_per_sec, Some(200.0));
    }

    #[test]
    fn health_rtt_comes_from_the_probe() {
        let mut config = health_config(None);
        config.measured_rtt_ms = Some(Arc::new(AtomicU64::new(42.5f64.to_bits())));
        assert_eq!(health_packet_for_config(&config).active_server_rtt_ms, 42.5);
    }

    #[test]
    fn session_rates_divide_by_the_measured_window() {
        let hp = packet(&health_config(None), 500, 250, 5000.0);
        assert_eq!(hp.packets_received_per_sec, Some(100.0));
        assert_eq!(hp.packets_sent_per_sec, Some(50.0));
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
}
