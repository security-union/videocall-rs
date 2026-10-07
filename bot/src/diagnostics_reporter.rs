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

//! Periodic DiagnosticsPacket producer for the synthetic bot.
//!
//! Real browser clients (see `videocall-client/src/diagnostics/diagnostics_manager.rs`
//! `send_diagnostic_packets`) emit one `DiagnosticsPacket` per observed remote
//! peer per (audio, video) media type every heartbeat, so each broadcaster sees
//! downstream quality reports from every receiver. Without this, a bot-heavy
//! meeting would give senders only a single real-browser's report — AQ
//! controllers then collapse to `peers=1` and become blind in load tests.
//!
//! A Tokio task on its own cadence (default 500 ms, the browser's) drains a
//! per-sender byte accumulator from `InboundStats`, builds one
//! `DiagnosticsPacket` per tracked `(sender, media_type)`, wraps each in a
//! `PacketWrapper { packet_type = DIAGNOSTICS, .. }`, and emits it through the
//! shared outbound channel.
//!
//! The shape of each packet matches the browser exactly:
//! - `target_id` = the reporter's own user id (this bot)
//! - `sender_id` = the observed peer's user id (the stream subject)
//! - `media_type` = AUDIO or VIDEO
//! - `audio_metrics` / `video_metrics` with `fps_received` and `bitrate_kbps`
//!
//! This makes bots indistinguishable from browsers on the DIAGNOSTICS wire —
//! no bot-specific Prometheus labels or metrics are introduced.

use protobuf::{Message, MessageField};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::ClientConfig;
use crate::inbound_stats::{InboundStats, SenderHealthCounters};
use crate::transport::{MediaTypeLabel, OutboundFrame, OutboundFrameSender};
use videocall_types::protos::diagnostics_packet::{AudioMetrics, DiagnosticsPacket, VideoMetrics};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Configuration for the diagnostics reporter.
pub struct DiagnosticsReporterConfig {
    pub client_config: ClientConfig,
    /// DIAGNOSTICS cadence.
    pub interval: Duration,
    /// Report every tracked (peer, media) each tick, zeros included (browser behaviour).
    pub persistent_trackers: bool,
    /// Most video streams tracked at once; a browser tracks only the video it decodes.
    pub max_video_trackers: usize,
    /// `false`: no reporter task and no DIAGNOSTICS packets.
    pub enabled: bool,
    /// Shared counter for transport-level drops. Incremented when `try_send`
    /// fails on the outbound channel, contributing to the cumulative total
    /// reported in `HealthPacket.websocket_drops_total` /
    /// `datagram_drops_total`.
    pub transport_drops_counter: Arc<AtomicU64>,
}

/// Media kind of one DIAGNOSTICS tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DiagKind {
    Video,
    Audio,
}

impl DiagKind {
    fn media_type(self) -> MediaType {
        match self {
            DiagKind::Video => MediaType::VIDEO,
            DiagKind::Audio => MediaType::AUDIO,
        }
    }

    fn bytes(self, c: &SenderHealthCounters) -> u64 {
        match self {
            DiagKind::Video => c.video_bytes,
            DiagKind::Audio => c.audio_bytes,
        }
    }
}

/// One DIAGNOSTICS packet to emit: (observed peer, media kind, bytes in the window).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiagReport {
    pub sender: String,
    pub kind: DiagKind,
    pub bytes: u64,
}

/// Choose the DIAGNOSTICS packets for one window.
///
/// A tracker is created for each (peer, kind) the window carried bytes for.
/// With `persistent`, every tracker whose peer is still `known` is reported,
/// zero bytes included, as the browser does until the peer leaves; otherwise
/// only streams with bytes in this window are reported.
pub(crate) fn select_reports(
    trackers: &mut BTreeSet<(String, DiagKind)>,
    window: &HashMap<String, SenderHealthCounters>,
    known: &HashSet<String>,
    persistent: bool,
    max_video: usize,
) -> Vec<DiagReport> {
    trackers.retain(|(sender, _)| known.contains(sender) || window.contains_key(sender));
    let mut video = trackers
        .iter()
        .filter(|(_, kind)| *kind == DiagKind::Video)
        .count();
    let mut senders: Vec<&String> = window.keys().collect();
    senders.sort_unstable();
    for sender in senders {
        let c = &window[sender];
        if should_emit_video(c) && video < max_video {
            video += usize::from(trackers.insert((sender.clone(), DiagKind::Video)));
        }
        if should_emit_audio(c) {
            trackers.insert((sender.clone(), DiagKind::Audio));
        }
    }
    trackers
        .iter()
        .filter_map(|(sender, kind)| {
            let bytes = window.get(sender).map(|c| kind.bytes(c)).unwrap_or(0);
            (persistent || bytes > 0).then(|| DiagReport {
                sender: sender.clone(),
                kind: *kind,
                bytes,
            })
        })
        .collect()
}

/// One tick: drain the DIAGNOSTICS accumulator (not HEALTH's) and choose the
/// reports. A peer that left is no longer known, so its trackers are pruned.
pub(crate) fn collect_reports(
    stats: &Mutex<InboundStats>,
    trackers: &mut BTreeSet<(String, DiagKind)>,
    persistent: bool,
    max_video: usize,
) -> Vec<DiagReport> {
    let (window, known) = {
        let mut s = stats.lock().unwrap();
        s.evict_stale(crate::inbound_stats::PEER_SILENCE_EVICT);
        (s.drain_diagnostics_counters(), s.known_senders())
    };
    select_reports(trackers, &window, &known, persistent, max_video)
}

/// Spawn the per-peer DIAGNOSTICS reporter on its own cadence (`config.interval`).
///
/// The task runs until `quit` is set to `true`.
pub fn spawn_diagnostics_reporter(
    config: DiagnosticsReporterConfig,
    stats: Arc<Mutex<InboundStats>>,
    packet_sender: OutboundFrameSender,
    quit: Arc<AtomicBool>,
) {
    if !config.enabled {
        info!(
            "Diagnostics disabled for {}: no DIAGNOSTICS packets",
            config.client_config.user_id
        );
        return;
    }
    tokio::spawn(async move {
        let mut interval = time::interval(config.interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        interval.tick().await;
        let mut window_start = Instant::now();
        let mut trackers: BTreeSet<(String, DiagKind)> = BTreeSet::new();

        info!(
            "Diagnostics reporter started for {} in meeting {} (every {:?}, persistent={})",
            config.client_config.user_id,
            config.client_config.meeting_id,
            config.interval,
            config.persistent_trackers
        );

        let user_id = config.client_config.user_id.clone();
        let user_id_bytes = user_id.as_bytes().to_vec();

        loop {
            interval.tick().await;

            if quit.load(Ordering::Relaxed) {
                break;
            }

            let window_ms = window_start.elapsed().as_secs_f64() * 1000.0;
            window_start = Instant::now();

            let reports = collect_reports(
                &stats,
                &mut trackers,
                config.persistent_trackers,
                config.max_video_trackers,
            );

            let timestamp_ms = now_millis();
            let mut emitted = 0usize;
            for r in &reports {
                let kbps = bytes_to_kbps(r.bytes, window_ms);
                match build_wrapper(
                    &user_id,
                    &user_id_bytes,
                    &r.sender,
                    timestamp_ms,
                    r.kind.media_type(),
                    kbps,
                ) {
                    Ok(bytes) => {
                        if try_emit(&packet_sender, bytes, &config.transport_drops_counter) {
                            emitted += 1;
                        }
                    }
                    Err(e) => warn!(
                        "Failed to build {:?} diagnostics for {}->{}: {}",
                        r.kind, user_id, r.sender, e
                    ),
                }
            }

            debug!(
                "Emitted {} diagnostics packets for {} ({} trackers)",
                emitted,
                user_id,
                trackers.len(),
            );
        }

        info!("Diagnostics reporter stopped for {}", user_id);
    });
}

/// Whether to emit the VIDEO half. Liveness reads ARRIVAL (bytes): both frame
/// counts are rung-filtered (#2206 video, #2244 audio) and sit at zero for a
/// whole availability window after a shed.
pub(crate) fn should_emit_video(counters: &SenderHealthCounters) -> bool {
    counters.video_bytes > 0
}

pub(crate) fn should_emit_audio(counters: &SenderHealthCounters) -> bool {
    counters.audio_bytes > 0
}

/// Build a serialized `PacketWrapper { packet_type = DIAGNOSTICS, ... }`
/// containing a single `DiagnosticsPacket` for the given `(sender_id,
/// media_type)` pair. The reporter's own user id becomes `target_id`.
fn build_wrapper(
    user_id: &str,
    user_id_bytes: &[u8],
    sender_id: &str,
    timestamp_ms: u64,
    media_type: MediaType,
    bitrate_kbps: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut packet = DiagnosticsPacket::new();
    // Match browser semantics: target_id is the reporter (self) and
    // sender_id is the observed peer (subject of the report).
    packet.target_id = user_id.to_string();
    packet.sender_id = sender_id.to_string();
    packet.timestamp_ms = timestamp_ms;
    packet.media_type = media_type.into();

    match media_type {
        MediaType::VIDEO => {
            let mut vm = VideoMetrics::new();
            // NOTE(#1184): VideoMetrics.fps_received was removed (dead
            // receiver-FPS telemetry — written but never consumed). Only the
            // live bitrate signal remains.
            vm.bitrate_kbps = bitrate_kbps;
            packet.video_metrics = MessageField::some(vm);
        }
        MediaType::AUDIO => {
            let mut am = AudioMetrics::new();
            am.bitrate_kbps = bitrate_kbps;
            packet.audio_metrics = MessageField::some(am);
        }
        _ => {}
    }

    let diag_bytes = packet.write_to_bytes()?;

    let wrapper = PacketWrapper {
        packet_type: PacketType::DIAGNOSTICS.into(),
        user_id: user_id_bytes.to_vec(),
        data: diag_bytes,
        ..Default::default()
    };

    Ok(wrapper.write_to_bytes()?)
}

/// `try_send` the serialized wrapper and log-throttle drops, mirroring the
/// health reporter's dropped-send pattern.
fn try_emit(
    packet_sender: &OutboundFrameSender,
    bytes: Vec<u8>,
    transport_drops: &AtomicU64,
) -> bool {
    let frame = OutboundFrame::new(MediaTypeLabel::Diagnostics, bytes);
    if let Err(_e) = packet_sender.try_send(frame) {
        static DIAG_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
        let count = DIAG_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        // Also increment the shared transport drops counter so the
        // cumulative total includes diagnostics packet drops.
        transport_drops.fetch_add(1, Ordering::Relaxed);
        if count % 100 == 1 {
            warn!(
                "Dropped diagnostics packets due to full send channel (total: {})",
                count,
            );
        }
        false
    } else {
        true
    }
}

/// Convert a byte count observed over `window_ms` to kbps, saturating at
/// `u32::MAX` to match the protobuf field type.
fn bytes_to_kbps(bytes: u64, window_ms: f64) -> u32 {
    let kbps = bytes as f64 * 8.0 / window_ms.max(1.0);
    kbps.min(u32::MAX as f64) as u32
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound_stats::test_packets;
    use crate::inbound_stats::SenderHealthCounters;

    async fn diagnostics_sent(enabled: bool) -> usize {
        let stats = Arc::new(Mutex::new(InboundStats::default()));
        stats
            .lock()
            .unwrap()
            .record_packet("me", &test_packets::media("alice", MediaType::AUDIO));
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let config = DiagnosticsReporterConfig {
            client_config: crate::config::ClientConfig::for_role(
                "me".into(),
                "room".into(),
                crate::config::Role::Viewer,
                Duration::from_secs(5),
            ),
            interval: Duration::from_millis(20),
            persistent_trackers: true,
            max_video_trackers: 12,
            enabled,
            transport_drops_counter: Arc::new(AtomicU64::new(0)),
        };
        let quit = Arc::new(AtomicBool::new(false));
        spawn_diagnostics_reporter(config, stats, tx.into(), Arc::clone(&quit));
        tokio::time::sleep(Duration::from_millis(150)).await;
        quit.store(true, Ordering::Relaxed);
        let mut sent = 0;
        while let Ok(frame) = rx.try_recv() {
            sent += usize::from(frame.kind == MediaTypeLabel::Diagnostics);
        }
        sent
    }

    #[tokio::test]
    async fn diagnostics_off_sends_no_packets() {
        assert!(diagnostics_sent(true).await > 0, "precondition: on sends");
        assert_eq!(diagnostics_sent(false).await, 0);
    }

    #[test]
    fn a_tick_reads_its_own_window_after_health_drained() {
        let stats = Mutex::new(InboundStats::default());
        stats
            .lock()
            .unwrap()
            .record_packet("me", &test_packets::media("alice", MediaType::AUDIO));
        let _ = stats.lock().unwrap().take_health_total();
        let reports = collect_reports(&stats, &mut BTreeSet::new(), true, usize::MAX);
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].bytes > 0,
            "HEALTH's drain must not empty DIAGNOSTICS"
        );
    }

    #[test]
    fn trackers_stop_when_the_peer_leaves_not_after_idling() {
        let stats = Mutex::new(InboundStats::default());
        let mut trackers = BTreeSet::new();
        for kind in [MediaType::AUDIO, MediaType::VIDEO] {
            stats
                .lock()
                .unwrap()
                .record_packet("me", &test_packets::media("alice", kind));
        }
        assert_eq!(
            collect_reports(&stats, &mut trackers, true, usize::MAX).len(),
            2
        );
        let idle = collect_reports(&stats, &mut trackers, true, usize::MAX);
        assert_eq!(
            idle.len(),
            2,
            "an idle but present peer keeps reporting zeros"
        );
        stats
            .lock()
            .unwrap()
            .record_packet("me", &test_packets::participant_left("alice"));
        assert!(collect_reports(&stats, &mut trackers, true, usize::MAX).is_empty());
    }

    #[test]
    fn trackers_stop_once_the_peer_is_silent_past_the_browser_window() {
        let stats = Mutex::new(InboundStats::default());
        let mut trackers = BTreeSet::new();
        stats
            .lock()
            .unwrap()
            .record_packet("me", &test_packets::media("alice", MediaType::AUDIO));
        assert_eq!(
            collect_reports(&stats, &mut trackers, true, usize::MAX).len(),
            1
        );
        stats
            .lock()
            .unwrap()
            .backdate_sender("alice", Duration::from_secs(16));
        assert!(collect_reports(&stats, &mut trackers, true, usize::MAX).is_empty());
    }

    #[test]
    fn video_wrapper_has_correct_identity_and_metrics() {
        let counters = SenderHealthCounters {
            audio_bytes: 4_000,
            video_bytes: 125_000,
        };
        let user_id = "bot-1";
        let sender_id = "alice";

        let bytes = build_wrapper(
            user_id,
            user_id.as_bytes(),
            sender_id,
            1_700_000_000_000,
            MediaType::VIDEO,
            bytes_to_kbps(counters.video_bytes, 1000.0),
        )
        .expect("build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper");
        assert_eq!(
            wrapper.packet_type.enum_value(),
            Ok(PacketType::DIAGNOSTICS)
        );
        assert_eq!(wrapper.user_id, user_id.as_bytes());

        let diag = DiagnosticsPacket::parse_from_bytes(&wrapper.data).expect("diag");
        assert_eq!(diag.target_id, user_id);
        assert_eq!(diag.sender_id, sender_id);
        assert_eq!(diag.media_type.enum_value(), Ok(MediaType::VIDEO));
        let vm = diag.video_metrics.as_ref().expect("video metrics present");
        // 125_000 bytes/s * 8 / 1000 = 1000 kbps
        assert_eq!(vm.bitrate_kbps, 1000);
        assert!(diag.audio_metrics.is_none());
    }

    #[test]
    fn audio_wrapper_populates_audio_metrics_only() {
        let counters = SenderHealthCounters {
            audio_bytes: 5_000,
            video_bytes: 0,
        };
        let bytes = build_wrapper(
            "bot-1",
            b"bot-1",
            "alice",
            1,
            MediaType::AUDIO,
            bytes_to_kbps(counters.audio_bytes, 1000.0),
        )
        .expect("build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper");
        let diag = DiagnosticsPacket::parse_from_bytes(&wrapper.data).expect("diag");
        assert_eq!(diag.media_type.enum_value(), Ok(MediaType::AUDIO));
        let am = diag.audio_metrics.as_ref().expect("audio metrics present");
        // 5_000 * 8 / 1000 = 40 kbps
        assert_eq!(am.bitrate_kbps, 40);
        assert!(diag.video_metrics.is_none());
    }

    #[test]
    fn bytes_to_kbps_handles_zero_and_saturation() {
        assert_eq!(bytes_to_kbps(0, 1000.0), 0);
        assert_eq!(bytes_to_kbps(125, 1000.0), 1);
        // Huge byte count should saturate at u32::MAX rather than wrapping.
        assert_eq!(bytes_to_kbps(u64::MAX, 1000.0), u32::MAX);
    }

    #[test]
    fn bytes_to_kbps_divides_by_the_window() {
        // 62_500 bytes in a 500 ms DIAGNOSTICS window is 1000 kbps.
        assert_eq!(bytes_to_kbps(62_500, 500.0), 1000);
    }

    fn window(entries: &[(&str, u64, u64)]) -> HashMap<String, SenderHealthCounters> {
        entries
            .iter()
            .map(|(name, audio, video)| {
                (
                    name.to_string(),
                    SenderHealthCounters {
                        audio_bytes: *audio,
                        video_bytes: *video,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn persistent_trackers_keep_reporting_an_idle_stream_until_the_peer_is_gone() {
        let mut trackers = BTreeSet::new();
        let known: HashSet<String> = vec!["alice".to_string()].into_iter().collect();
        let first = select_reports(
            &mut trackers,
            &window(&[("alice", 100, 200)]),
            &known,
            true,
            usize::MAX,
        );
        assert_eq!(first.len(), 2);
        let idle = select_reports(&mut trackers, &window(&[]), &known, true, usize::MAX);
        assert_eq!(
            idle.len(),
            2,
            "browser reports idle trackers with zero bitrate"
        );
        assert!(idle.iter().all(|r| r.bytes == 0));
        let gone = select_reports(
            &mut trackers,
            &window(&[]),
            &HashSet::new(),
            true,
            usize::MAX,
        );
        assert!(gone.is_empty(), "a peer no longer known drops its trackers");
    }

    #[test]
    fn video_trackers_stop_at_the_browser_decode_cap_while_audio_is_all_tracked() {
        let mut trackers = BTreeSet::new();
        let senders = [("a", 10, 10), ("b", 10, 10), ("c", 10, 10)];
        let known: HashSet<String> = senders.iter().map(|s| s.0.to_string()).collect();
        let reports = select_reports(&mut trackers, &window(&senders), &known, true, 2);
        let count = |kind| reports.iter().filter(|r| r.kind == kind).count();
        assert_eq!((count(DiagKind::Video), count(DiagKind::Audio)), (2, 3));
        let gone: HashSet<String> = ["b", "c"].iter().map(|s| s.to_string()).collect();
        let after_leave = select_reports(&mut trackers, &window(&senders[1..]), &gone, true, 2);
        assert_eq!(
            after_leave
                .iter()
                .filter(|r| r.kind == DiagKind::Video)
                .count(),
            2,
            "a slot freed by a departed peer goes to another video stream"
        );
    }

    #[test]
    fn legacy_mode_reports_only_active_streams() {
        let mut trackers = BTreeSet::new();
        let known: HashSet<String> = vec!["alice".to_string()].into_iter().collect();
        let _ = select_reports(
            &mut trackers,
            &window(&[("alice", 100, 200)]),
            &known,
            false,
            usize::MAX,
        );
        let reports = select_reports(
            &mut trackers,
            &window(&[("alice", 0, 50)]),
            &known,
            false,
            usize::MAX,
        );
        assert_eq!(
            reports,
            vec![DiagReport {
                sender: "alice".into(),
                kind: DiagKind::Video,
                bytes: 50
            }]
        );
    }

    /// The state #2206 creates: the shed top rung is still inside the availability
    /// window, so the rung-filtered frame count is 0 while bytes keep arriving.
    const POST_SHED: SenderHealthCounters = SenderHealthCounters {
        audio_bytes: 0,
        video_bytes: 2000,
    };

    /// The (peer, media) streams the production selection tracks for one window.
    fn tracked(counters: SenderHealthCounters) -> Vec<DiagKind> {
        let known: HashSet<String> = std::iter::once("alice".to_string()).collect();
        let window: HashMap<String, SenderHealthCounters> =
            std::iter::once(("alice".to_string(), counters)).collect();
        select_reports(&mut BTreeSet::new(), &window, &known, true, 12)
            .into_iter()
            .map(|r| r.kind)
            .collect()
    }

    #[test]
    fn a_shed_ladder_still_emits_because_liveness_reads_arrival() {
        assert_eq!(tracked(POST_SHED), vec![DiagKind::Video]);
    }

    #[test]
    fn a_silent_sender_emits_nothing() {
        assert!(tracked(SenderHealthCounters::default()).is_empty());
    }

    #[test]
    fn an_audio_only_sender_past_a_rung_shed_still_emits_the_audio_half_only() {
        let shed = SenderHealthCounters {
            audio_bytes: 5000,
            ..Default::default()
        };
        assert_eq!(tracked(shed), vec![DiagKind::Audio]);
    }
}
