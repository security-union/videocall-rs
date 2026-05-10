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

//! Receiver-side packet quality diagnostics shared by WebSocket and WebTransport clients.
//!
//! Parses every inbound `PacketWrapper` → `MediaPacket`, tracks per-sender sequence
//! numbers, measures inter-arrival variability, and computes A/V sync drift.
//! Reports a summary line at `INFO` level every 10 seconds.

use protobuf::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use crate::aq_controller::BotAq;
use crate::keyframe_requester::KeyframeRequester;
use crate::rtt_probe::RttProbeState;

#[cfg(feature = "metrics")]
use crate::metrics_server::BotMetrics;

/// Per-sender counters accumulated between health report drains.
#[derive(Default, Clone)]
pub struct SenderHealthCounters {
    pub audio_packets: u64,
    pub video_packets: u64,
    pub audio_bytes: u64,
    pub video_bytes: u64,
}

/// Tracks inbound packet statistics for quality-of-service diagnostics.
#[derive(Default)]
pub struct InboundStats {
    audio_packets: u64,
    video_packets: u64,
    video_keyframes: u64,
    heartbeat_packets: u64,
    other_packets: u64,
    audio_bytes: u64,
    video_bytes: u64,
    /// Highest audio sequence number seen per sender (for gap detection under reorder).
    max_audio_seq: HashMap<String, u64>,
    /// Highest video sequence number seen per sender (for gap detection under reorder).
    max_video_seq: HashMap<String, u64>,
    audio_seq_gaps: u64,
    video_seq_gaps: u64,
    /// Arrival times for inter-arrival variability calculation.
    video_arrivals: Vec<f64>,
    audio_arrivals: Vec<f64>,
    // A/V sync dropped: browser audio uses Date.now() ms, video uses EncodedVideoChunk µs — cross-unit delta is meaningless. Re-add when browser wire format is unified.
    parse_errors: u64,
    /// Per-sender counters for health reporting (accumulated between drains).
    health_counters: HashMap<String, SenderHealthCounters>,
    /// Total inbound packets since last health drain (all types).
    health_total_packets: u64,
    /// Snapshot of the most recently drained health-counter window, kept so
    /// secondary consumers (e.g. the diagnostics reporter) can read the same
    /// window the health reporter emitted without double-draining and zeroing
    /// the live counters between producers.
    last_drain_snapshot: HashMap<String, SenderHealthCounters>,
    /// Last time each sender was seen — used to evict stale entries.
    last_seen: HashMap<String, Instant>,
    /// Intern map: raw user_id bytes → owned String to avoid per-packet allocation.
    sender_names: HashMap<Vec<u8>, String>,
    /// Optional adaptive-quality controller. When set, inbound DIAGNOSTICS
    /// packets are forwarded here so the bot's encoders can adapt.
    aq: Option<Arc<BotAq>>,
    /// Number of DIAGNOSTICS packets that failed to parse since the last reset.
    diagnostics_parse_errors: u64,
    /// Optional RTT probe state. When set, echoed RTT packets are routed here
    /// to compute real round-trip time instead of being counted as media.
    rtt_probe: Option<Arc<RttProbeState>>,
    /// Optional keyframe requester. When set, new peers trigger a
    /// KEYFRAME_REQUEST for VIDEO the first time they are observed.
    keyframe_requester: Option<KeyframeRequester>,
    /// Optional Prometheus metrics handle. When set, every inbound packet
    /// increments `bot_packets_received_total` (labeled by media_type) and
    /// parse failures increment `bot_packets_parsed_error_total`.
    #[cfg(feature = "metrics")]
    metrics: Option<InboundMetrics>,
}

/// Label bundle for inbound packet metrics.
#[cfg(feature = "metrics")]
struct InboundMetrics {
    metrics: Arc<BotMetrics>,
    bot: String,
    meeting: String,
}

impl InboundStats {
    /// Attach an adaptive-quality controller. Once set, every inbound
    /// DIAGNOSTICS packet is forwarded to it so the bot's encoders can adapt.
    pub fn set_aq(&mut self, aq: Arc<BotAq>) {
        self.aq = Some(aq);
    }

    /// Attach an RTT probe state. When set, echoed RTT packets from the relay
    /// are routed to `RttProbeState::record_echo` instead of being counted as
    /// generic media.
    pub fn set_rtt_probe(&mut self, state: Arc<RttProbeState>) {
        self.rtt_probe = Some(state);
    }

    /// Attach a keyframe requester. When set, newly discovered peers trigger
    /// a KEYFRAME_REQUEST for VIDEO.
    pub fn set_keyframe_requester(&mut self, requester: KeyframeRequester) {
        self.keyframe_requester = Some(requester);
    }

    /// Install (or replace) the Prometheus metrics handle. Calls made before
    /// `set_metrics` are uncounted — we intentionally do not buffer on the
    /// hot path.
    #[cfg(feature = "metrics")]
    pub fn set_metrics(&mut self, metrics: Arc<BotMetrics>, bot: String, meeting: String) {
        self.metrics = Some(InboundMetrics {
            metrics,
            bot,
            meeting,
        });
    }

    /// Increment `bot_packets_received_total{media_type=…}`. No-op when
    /// metrics are off or unbound.
    #[cfg(feature = "metrics")]
    fn bump_received(&self, media_type: &str) {
        if let Some(m) = &self.metrics {
            m.metrics
                .packets_received_total
                .with_label_values(&[m.bot.as_str(), m.meeting.as_str(), media_type])
                .inc();
        }
    }

    /// Increment `bot_packets_parsed_error_total{stage=…}`.
    #[cfg(feature = "metrics")]
    fn bump_parse_error(&self, stage: &str) {
        if let Some(m) = &self.metrics {
            m.metrics
                .packets_parsed_error_total
                .with_label_values(&[m.bot.as_str(), m.meeting.as_str(), stage])
                .inc();
        }
    }

    pub fn record_packet(&mut self, my_user_id: &str, data: &[u8]) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        self.health_total_packets += 1;

        let Ok(wrapper) = PacketWrapper::parse_from_bytes(data) else {
            self.parse_errors += 1;
            #[cfg(feature = "metrics")]
            self.bump_parse_error("wrapper");
            return;
        };

        // DIAGNOSTICS packets are fed to the AQ controller so the bot can
        // react to downstream quality signals like a real browser client.
        // We deliberately intercept this before the MEDIA early-return so the
        // relay's diagnostic broadcasts stop being silently dropped.
        //
        // Only forward packets about *this bot's* own stream — match the
        // browser's `SenderDiagnosticManager` which filters on
        // `sender_id == self.userid` before feeding the encoder. Without this
        // filter, unrelated peer→peer reports would be mixed into this bot's
        // AQ controller's per-reporter window.
        if wrapper.packet_type.enum_value() == Ok(PacketType::DIAGNOSTICS) {
            match DiagnosticsPacket::parse_from_bytes(&wrapper.data) {
                Ok(diag) => {
                    if diag.sender_id == my_user_id {
                        if let Some(ref aq) = self.aq {
                            aq.process_diagnostics(diag);
                        }
                    }
                }
                Err(e) => {
                    self.diagnostics_parse_errors += 1;
                    #[cfg(feature = "metrics")]
                    self.bump_parse_error("diagnostics");
                    // Rate-limit so a malformed peer cannot spam the log.
                    if self.diagnostics_parse_errors.is_multiple_of(100) {
                        warn!(
                            "Failed to parse DIAGNOSTICS packet (total: {}): {}",
                            self.diagnostics_parse_errors, e
                        );
                    }
                }
            }
            self.other_packets += 1;
            #[cfg(feature = "metrics")]
            self.bump_received("diagnostics");
            return;
        }

        if wrapper.packet_type.enum_value() != Ok(PacketType::MEDIA) {
            self.other_packets += 1;
            #[cfg(feature = "metrics")]
            self.bump_received("other");
            return;
        }

        let Ok(media) = MediaPacket::parse_from_bytes(&wrapper.data) else {
            self.parse_errors += 1;
            #[cfg(feature = "metrics")]
            self.bump_parse_error("media");
            return;
        };

        // Intercept echoed RTT packets BEFORE normal media accounting.
        // The relay echoes the entire PacketWrapper back verbatim when
        // media_type == RTT, so our own probe comes back with our user_id
        // in wrapper.user_id. Route to the RTT probe state for RTT calc.
        if media.media_type.enum_value() == Ok(MediaType::RTT) {
            if let Some(ref rtt_state) = self.rtt_probe {
                rtt_state.record_echo(media.timestamp);
                debug!("RTT echo received, timestamp={:.1}", media.timestamp);
            }
            #[cfg(feature = "metrics")]
            self.bump_received("rtt");
            self.other_packets += 1;
            return;
        }

        // The relay populates wrapper.user_id but strips media.user_id,
        // so use the wrapper-level user_id for per-sender tracking.
        let sender = self.intern_sender(&wrapper.user_id).to_owned();

        // Update last-seen time for stale entry eviction.
        self.last_seen.insert(sender.clone(), Instant::now());

        // Notify keyframe requester about newly seen peers.
        if let Some(ref mut kr) = self.keyframe_requester {
            kr.on_peer_seen(&sender);
        }

        match media.media_type.enum_value() {
            Ok(MediaType::AUDIO) => {
                #[cfg(feature = "metrics")]
                self.bump_received("audio");
                self.audio_packets += 1;
                self.audio_bytes += media.data.len() as u64;
                self.audio_arrivals.push(now_ms);

                // Accumulate health counters for this sender
                let hc = self.health_counters.entry(sender.clone()).or_default();
                hc.audio_packets += 1;
                hc.audio_bytes += media.data.len() as u64;

                if let Some(meta) = media.audio_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(&max_seen) = self.max_audio_seq.get(&sender) {
                        if seq > max_seen + 1 {
                            // Gap: packets between max_seen and seq are missing
                            self.audio_seq_gaps += seq - max_seen - 1;
                        }
                        // Only update if this is a new high-water mark
                        if seq > max_seen {
                            self.max_audio_seq.insert(sender.clone(), seq);
                        }
                        // seq <= max_seen means reorder/duplicate — do not count as gap
                    } else {
                        // First packet from this sender
                        self.max_audio_seq.insert(sender.clone(), seq);
                    }
                }
            }
            Ok(MediaType::VIDEO) => {
                #[cfg(feature = "metrics")]
                self.bump_received("video");
                self.video_packets += 1;
                self.video_bytes += media.data.len() as u64;
                self.video_arrivals.push(now_ms);

                // Accumulate health counters for this sender
                let hc = self.health_counters.entry(sender.clone()).or_default();
                hc.video_packets += 1;
                hc.video_bytes += media.data.len() as u64;

                if media.frame_type == "key" {
                    self.video_keyframes += 1;
                }

                if let Some(meta) = media.video_metadata.as_ref() {
                    let seq = meta.sequence;
                    if let Some(&max_seen) = self.max_video_seq.get(&sender) {
                        if seq > max_seen + 1 {
                            // Gap: packets between max_seen and seq are missing
                            self.video_seq_gaps += seq - max_seen - 1;
                        }
                        // Only update if this is a new high-water mark
                        if seq > max_seen {
                            self.max_video_seq.insert(sender.clone(), seq);
                        }
                        // seq <= max_seen means reorder/duplicate — do not count as gap
                    } else {
                        // First packet from this sender
                        self.max_video_seq.insert(sender.clone(), seq);
                    }
                }
            }
            Ok(MediaType::HEARTBEAT) => {
                #[cfg(feature = "metrics")]
                self.bump_received("health");
                self.heartbeat_packets += 1;
            }
            _ => {
                #[cfg(feature = "metrics")]
                self.bump_received("other");
                self.other_packets += 1;
            }
        }
    }

    /// Inter-arrival time standard deviation (not RFC 3550 jitter).
    /// Measures variability in packet arrival timing as the standard deviation
    /// of consecutive inter-arrival deltas.
    fn interarrival_stddev_ms(arrivals: &[f64]) -> f64 {
        if arrivals.len() < 2 {
            return 0.0;
        }
        let deltas: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
        variance.sqrt()
    }

    pub fn report(&self, user_id: &str) {
        let audio_iastddev = Self::interarrival_stddev_ms(&self.audio_arrivals);
        let video_iastddev = Self::interarrival_stddev_ms(&self.video_arrivals);

        info!(
            "[{}] RX STATS (10s): audio={} pkts ({:.0} KB, ia_stddev={:.1}ms, gaps={}), \
             video={} pkts ({} key, {:.0} KB, ia_stddev={:.1}ms, gaps={}), \
             heartbeat={}, errors={}",
            user_id,
            self.audio_packets,
            self.audio_bytes as f64 / 1024.0,
            audio_iastddev,
            self.audio_seq_gaps,
            self.video_packets,
            self.video_keyframes,
            self.video_bytes as f64 / 1024.0,
            video_iastddev,
            self.video_seq_gaps,
            self.heartbeat_packets,
            self.parse_errors,
        );
    }

    pub fn reset(&mut self) {
        // Preserve health counters across diagnostic resets — they are
        // drained independently by the health reporter on a 1s cadence.
        // Also preserve last_seen, max_*_seq, last_*_ts, and sender_names
        // since they track cross-window state. They are evicted by evict_stale().
        let health_counters = std::mem::take(&mut self.health_counters);
        let health_total = self.health_total_packets;
        let last_drain_snapshot = std::mem::take(&mut self.last_drain_snapshot);
        let last_seen = std::mem::take(&mut self.last_seen);
        let max_audio_seq = std::mem::take(&mut self.max_audio_seq);
        let max_video_seq = std::mem::take(&mut self.max_video_seq);
        let sender_names = std::mem::take(&mut self.sender_names);
        let aq = self.aq.take();
        let rtt_probe = self.rtt_probe.take();
        let keyframe_requester = self.keyframe_requester.take();
        #[cfg(feature = "metrics")]
        let metrics = self.metrics.take();
        *self = Self::default();
        self.health_counters = health_counters;
        self.health_total_packets = health_total;
        self.last_drain_snapshot = last_drain_snapshot;
        self.last_seen = last_seen;
        self.max_audio_seq = max_audio_seq;
        self.max_video_seq = max_video_seq;
        self.sender_names = sender_names;
        self.aq = aq;
        self.rtt_probe = rtt_probe;
        self.keyframe_requester = keyframe_requester;
        #[cfg(feature = "metrics")]
        {
            self.metrics = metrics;
        }
    }

    /// Remove entries from ALL per-sender maps for senders not seen within `max_age`.
    /// Call this periodically (e.g. from the 10s reporting tick) to bound memory.
    pub fn evict_stale(&mut self, max_age: Duration) {
        let cutoff = Instant::now() - max_age;
        let stale_senders: Vec<String> = self
            .last_seen
            .iter()
            .filter(|(_, &ts)| ts < cutoff)
            .map(|(k, _)| k.clone())
            .collect();

        for sender in &stale_senders {
            self.last_seen.remove(sender);
            self.max_audio_seq.remove(sender);
            self.max_video_seq.remove(sender);
            self.health_counters.remove(sender);
            self.last_drain_snapshot.remove(sender);
        }

        // Also evict from the intern map — find Vec<u8> keys whose String value
        // matches a stale sender.
        if !stale_senders.is_empty() {
            self.sender_names.retain(|_, v| !stale_senders.contains(v));
        }
    }

    /// Drain per-sender health counters accumulated since the last drain.
    /// Returns `(per_sender_counters, total_packets)` and resets both to zero.
    ///
    /// Before clearing, the drained per-sender map is cloned into
    /// `last_drain_snapshot` so secondary consumers (e.g. the diagnostics
    /// reporter) can read the *same* one-second window the health reporter
    /// emitted — a single source of truth for per-sender rate counters.
    pub fn drain_health_counters(&mut self) -> (HashMap<String, SenderHealthCounters>, u64) {
        let counters = std::mem::take(&mut self.health_counters);
        let total = self.health_total_packets;
        self.health_total_packets = 0;
        self.last_drain_snapshot = counters.clone();
        (counters, total)
    }

    /// Non-destructive snapshot of the last drained health-counter window.
    ///
    /// The diagnostics reporter calls this each tick to emit
    /// `DiagnosticsPacket`s over the same ~1-second window the health reporter
    /// already observed. Returns an empty map before the first drain.
    pub fn snapshot_diagnostics_counters(&self) -> HashMap<String, SenderHealthCounters> {
        self.last_drain_snapshot.clone()
    }

    /// Convert raw user_id bytes to a String, reusing previous conversions
    /// to avoid per-packet allocation from `String::from_utf8_lossy`.
    fn intern_sender(&mut self, raw: &[u8]) -> &str {
        if !self.sender_names.contains_key(raw) {
            self.sender_names
                .insert(raw.to_vec(), String::from_utf8_lossy(raw).into_owned());
        }
        &self.sender_names[raw]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::media_packet::media_packet::MediaType;
    use videocall_types::protos::media_packet::{AudioMetadata, MediaPacket, VideoMetadata};
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// Build a serialized PacketWrapper containing a MediaPacket for testing.
    fn make_media_packet(sender: &str, media_type: MediaType, seq: u64, timestamp: f64) -> Vec<u8> {
        let mut media = MediaPacket::new();
        media.media_type = media_type.into();
        media.data = vec![0u8; 100]; // dummy payload
        media.timestamp = timestamp;

        match media_type {
            MediaType::AUDIO => {
                media.audio_metadata = Some(AudioMetadata {
                    sequence: seq,
                    ..Default::default()
                })
                .into();
            }
            MediaType::VIDEO => {
                media.video_metadata = Some(VideoMetadata {
                    sequence: seq,
                    ..Default::default()
                })
                .into();
            }
            _ => {}
        }

        let wrapper = PacketWrapper {
            user_id: sender.as_bytes().to_vec(),
            packet_type: PacketType::MEDIA.into(),
            data: media.write_to_bytes().unwrap(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap()
    }

    #[test]
    fn test_gap_detection() {
        let mut stats = InboundStats::default();

        // Send sequential audio packets: 0, 1, 2
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::AUDIO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        assert_eq!(
            stats.audio_seq_gaps, 0,
            "sequential packets should have no gaps"
        );

        // Skip seq 3, send seq 5 (gap of 1 missing packet: seq 4 is also missing = gap of 2)
        let data = make_media_packet("alice", MediaType::AUDIO, 5, 1005.0);
        stats.record_packet("bot", &data);
        assert_eq!(
            stats.audio_seq_gaps, 2,
            "skipping 3->5 should detect 2 gap packets"
        );

        // Same test for video
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::VIDEO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        assert_eq!(stats.video_seq_gaps, 0);

        let data = make_media_packet("alice", MediaType::VIDEO, 5, 1005.0);
        stats.record_packet("bot", &data);
        assert_eq!(stats.video_seq_gaps, 2);
    }

    #[test]
    fn test_reorder_no_false_gap() {
        let mut stats = InboundStats::default();

        // Send video seq 0, 1, 3, 2 — the 3 before 2 is a reorder
        let data = make_media_packet("bob", MediaType::VIDEO, 0, 1000.0);
        stats.record_packet("bot", &data);

        let data = make_media_packet("bob", MediaType::VIDEO, 1, 1001.0);
        stats.record_packet("bot", &data);

        // seq 3 arrives before seq 2 — gap of 1 (seq 2 missing at this point)
        let data = make_media_packet("bob", MediaType::VIDEO, 3, 1003.0);
        stats.record_packet("bot", &data);
        assert_eq!(stats.video_seq_gaps, 1, "3 after 1 = 1 gap");

        // seq 2 arrives late (reorder) — should NOT create a false gap
        let data = make_media_packet("bob", MediaType::VIDEO, 2, 1002.0);
        stats.record_packet("bot", &data);
        assert_eq!(
            stats.video_seq_gaps, 1,
            "late seq 2 should not increase gap count"
        );

        // max_seen should still be 3
        assert_eq!(stats.max_video_seq.get("bob"), Some(&3));
    }

    #[test]
    fn test_interarrival_stddev() {
        // Perfectly uniform arrivals should have zero stddev
        let uniform = vec![0.0, 10.0, 20.0, 30.0, 40.0];
        let stddev = InboundStats::interarrival_stddev_ms(&uniform);
        assert!(
            stddev < 0.001,
            "uniform arrivals should have ~0 stddev, got {}",
            stddev
        );

        // Alternating 10ms and 20ms inter-arrival deltas
        // deltas = [10, 20, 10, 20], mean = 15, variance = 25, stddev = 5
        let jittery = vec![0.0, 10.0, 30.0, 40.0, 60.0];
        let stddev = InboundStats::interarrival_stddev_ms(&jittery);
        assert!(
            (stddev - 5.0).abs() < 0.01,
            "expected stddev ~5.0, got {}",
            stddev
        );

        // Too few arrivals should return 0
        assert_eq!(InboundStats::interarrival_stddev_ms(&[]), 0.0);
        assert_eq!(InboundStats::interarrival_stddev_ms(&[42.0]), 0.0);
    }

    #[test]
    fn test_evict_stale() {
        let mut stats = InboundStats::default();

        // Record packets from two senders
        let data = make_media_packet("alice", MediaType::AUDIO, 0, 1000.0);
        stats.record_packet("bot", &data);
        let data = make_media_packet("bob", MediaType::VIDEO, 0, 1000.0);
        stats.record_packet("bot", &data);

        assert!(stats.last_seen.contains_key("alice"));
        assert!(stats.last_seen.contains_key("bob"));
        assert!(stats.max_audio_seq.contains_key("alice"));
        assert!(stats.max_video_seq.contains_key("bob"));

        // Backdate alice's last_seen to simulate being stale
        *stats.last_seen.get_mut("alice").unwrap() = Instant::now() - Duration::from_secs(120);

        // Evict with 60s threshold — alice should be removed, bob should remain
        stats.evict_stale(Duration::from_secs(60));

        assert!(
            !stats.last_seen.contains_key("alice"),
            "alice should be evicted"
        );
        assert!(!stats.max_audio_seq.contains_key("alice"));
        assert!(!stats.health_counters.contains_key("alice"));

        assert!(stats.last_seen.contains_key("bob"), "bob should remain");
        assert!(stats.max_video_seq.contains_key("bob"));
    }

    #[test]
    fn test_health_counters_drain() {
        let mut stats = InboundStats::default();

        // Record some packets
        for seq in 0..5 {
            let data = make_media_packet("alice", MediaType::AUDIO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }
        for seq in 0..3 {
            let data = make_media_packet("alice", MediaType::VIDEO, seq, 1000.0 + seq as f64);
            stats.record_packet("bot", &data);
        }

        assert_eq!(stats.health_total_packets, 8);

        // Drain
        let (counters, total) = stats.drain_health_counters();
        assert_eq!(total, 8);
        let alice = counters.get("alice").expect("alice should have counters");
        assert_eq!(alice.audio_packets, 5);
        assert_eq!(alice.video_packets, 3);

        // After drain, counters should be reset
        assert_eq!(stats.health_total_packets, 0);
        assert!(stats.health_counters.is_empty());

        // A second drain should return empty
        let (counters2, total2) = stats.drain_health_counters();
        assert_eq!(total2, 0);
        assert!(counters2.is_empty());
    }
}
