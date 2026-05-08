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
//! This module mirrors `health_reporter.rs`: a 1-second Tokio task that reads
//! per-sender counters from `InboundStats` (via the non-destructive
//! `snapshot_diagnostics_counters` so we don't double-drain the window the
//! health reporter already consumed), builds one `DiagnosticsPacket` per
//! `(sender, media_type)` pair, wraps each in a `PacketWrapper { packet_type =
//! DIAGNOSTICS, user_id = <bot's userid>, .. }`, and emits it through the
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::ClientConfig;
use crate::inbound_stats::{InboundStats, SenderHealthCounters};
use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::diagnostics_packet::{AudioMetrics, DiagnosticsPacket, VideoMetrics};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Configuration for the diagnostics reporter.
pub struct DiagnosticsReporterConfig {
    pub client_config: ClientConfig,
}

/// Spawn a diagnostics reporter task that emits per-peer
/// `DiagnosticsPacket`s once a second.
///
/// For every remote peer currently in the `InboundStats` snapshot, the task
/// emits up to two packets per tick — one for AUDIO (if any audio packets
/// arrived in the last window) and one for VIDEO (if any video packets
/// arrived) — matching the browser's per-`(peer, media_type)` cadence.
///
/// The task runs until `quit` is set to `true`.
pub fn spawn_diagnostics_reporter(
    config: DiagnosticsReporterConfig,
    stats: Arc<Mutex<InboundStats>>,
    packet_sender: Sender<OutboundFrame>,
    quit: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        // Skip the first immediate tick so the first report has a full second
        // of data (same reasoning as health_reporter.rs).
        interval.tick().await;

        info!(
            "Diagnostics reporter started for {} in meeting {}",
            config.client_config.user_id, config.client_config.meeting_id
        );

        loop {
            interval.tick().await;

            if quit.load(Ordering::Relaxed) {
                break;
            }

            // Read the last window *non-destructively* — the health reporter
            // owns the drain on this 1s cadence; we read the snapshot it left
            // behind.
            let sender_counters = {
                let s = stats.lock().unwrap();
                s.snapshot_diagnostics_counters()
            };

            if sender_counters.is_empty() {
                debug!(
                    "No peer counters to report diagnostics for {}",
                    config.client_config.user_id
                );
                continue;
            }

            let timestamp_ms = now_millis();
            let user_id = &config.client_config.user_id;
            let user_id_bytes = user_id.as_bytes().to_vec();

            let mut emitted = 0usize;
            for (sender_id, counters) in &sender_counters {
                // Skip senders with no observable traffic in the last window.
                if counters.video_packets == 0 && counters.audio_packets == 0 {
                    continue;
                }

                if counters.video_packets > 0 {
                    let bytes = match build_wrapper(
                        user_id,
                        &user_id_bytes,
                        sender_id,
                        timestamp_ms,
                        MediaType::VIDEO,
                        counters,
                    ) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(
                                "Failed to build VIDEO diagnostics for {}->{}: {}",
                                user_id, sender_id, e
                            );
                            continue;
                        }
                    };
                    if try_emit(&packet_sender, bytes) {
                        emitted += 1;
                    }
                }

                if counters.audio_packets > 0 {
                    let bytes = match build_wrapper(
                        user_id,
                        &user_id_bytes,
                        sender_id,
                        timestamp_ms,
                        MediaType::AUDIO,
                        counters,
                    ) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(
                                "Failed to build AUDIO diagnostics for {}->{}: {}",
                                user_id, sender_id, e
                            );
                            continue;
                        }
                    };
                    if try_emit(&packet_sender, bytes) {
                        emitted += 1;
                    }
                }
            }

            debug!(
                "Emitted {} diagnostics packets for {} ({} peers in window)",
                emitted,
                user_id,
                sender_counters.len(),
            );
        }

        info!(
            "Diagnostics reporter stopped for {}",
            config.client_config.user_id
        );
    });
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
    counters: &SenderHealthCounters,
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
            // Drain window is ~1s, so packet count ~ per-second rate.
            vm.fps_received = counters.video_packets as f32;
            vm.bitrate_kbps = bytes_to_kbps(counters.video_bytes);
            packet.video_metrics = MessageField::some(vm);
        }
        MediaType::AUDIO => {
            let mut am = AudioMetrics::new();
            am.fps_received = counters.audio_packets as f32;
            am.bitrate_kbps = bytes_to_kbps(counters.audio_bytes);
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
fn try_emit(packet_sender: &Sender<OutboundFrame>, bytes: Vec<u8>) -> bool {
    let frame = OutboundFrame::new(MediaTypeLabel::Diagnostics, bytes);
    if let Err(_e) = packet_sender.try_send(frame) {
        static DIAG_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
        let count = DIAG_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
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

/// Convert a byte count observed over a 1s window to kbps, saturating at
/// `u32::MAX` to match the protobuf field type.
fn bytes_to_kbps(bytes: u64) -> u32 {
    let bits = bytes.saturating_mul(8);
    let kbps = bits / 1000;
    kbps.min(u32::MAX as u64) as u32
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
    use crate::inbound_stats::SenderHealthCounters;

    #[test]
    fn video_wrapper_has_correct_identity_and_metrics() {
        let counters = SenderHealthCounters {
            audio_packets: 50,
            video_packets: 30,
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
            &counters,
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
        assert_eq!(vm.fps_received, 30.0f32);
        // 125_000 bytes/s * 8 / 1000 = 1000 kbps
        assert_eq!(vm.bitrate_kbps, 1000);
        assert!(diag.audio_metrics.is_none());
    }

    #[test]
    fn audio_wrapper_populates_audio_metrics_only() {
        let counters = SenderHealthCounters {
            audio_packets: 50,
            video_packets: 0,
            audio_bytes: 5_000,
            video_bytes: 0,
        };
        let bytes = build_wrapper("bot-1", b"bot-1", "alice", 1, MediaType::AUDIO, &counters)
            .expect("build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("wrapper");
        let diag = DiagnosticsPacket::parse_from_bytes(&wrapper.data).expect("diag");
        assert_eq!(diag.media_type.enum_value(), Ok(MediaType::AUDIO));
        let am = diag.audio_metrics.as_ref().expect("audio metrics present");
        assert_eq!(am.fps_received, 50.0f32);
        // 5_000 * 8 / 1000 = 40 kbps
        assert_eq!(am.bitrate_kbps, 40);
        assert!(diag.video_metrics.is_none());
    }

    #[test]
    fn bytes_to_kbps_handles_zero_and_saturation() {
        assert_eq!(bytes_to_kbps(0), 0);
        assert_eq!(bytes_to_kbps(125), 1);
        // Huge byte count should saturate at u32::MAX rather than wrapping.
        assert_eq!(bytes_to_kbps(u64::MAX), u32::MAX);
    }
}
