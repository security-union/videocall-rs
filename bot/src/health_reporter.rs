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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{ClientConfig, Transport};
use crate::inbound_stats::InboundStats;
use videocall_types::protos::health_packet::{
    HealthPacket as PbHealthPacket, NetEqStats as PbNetEqStats, PeerStats as PbPeerStats,
    VideoStats as PbVideoStats,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Configuration for the health reporter.
pub struct HealthReporterConfig {
    pub client_config: ClientConfig,
    pub transport: Transport,
    pub server_url: String,
}

/// Spawn a health reporter task that sends HealthPacket protos every second.
///
/// The task runs until `quit` is set to true. It drains per-sender counters
/// from the shared `InboundStats`, computes per-second rates, and sends the
/// resulting HealthPacket through `packet_sender`.
pub fn spawn_health_reporter(
    config: HealthReporterConfig,
    stats: Arc<Mutex<InboundStats>>,
    packet_sender: Sender<Vec<u8>>,
    quit: Arc<AtomicBool>,
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

            // Build HealthPacket proto.
            let packet_bytes = match build_health_packet(&config, &sender_counters, total_packets) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(
                        "Failed to build health packet for {}: {}",
                        config.client_config.user_id, e
                    );
                    continue;
                }
            };

            if let Err(_e) = packet_sender.try_send(packet_bytes) {
                static HEALTH_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
                let count = HEALTH_DROP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
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

    // Connection info
    hp.active_server_url = config.server_url.clone();
    hp.active_server_type = match config.transport {
        Transport::WebTransport => "webtransport".to_string(),
        Transport::WebSocket => "websocket".to_string(),
    };

    // Tab state: bot is always active and never throttled
    hp.is_tab_visible = true;
    hp.is_tab_throttled = false;

    // Bot does not adapt — report tier 0 (best)
    hp.adaptive_video_tier = Some(0);
    hp.adaptive_audio_tier = Some(0);
    hp.screen_sharing_active = Some(false);

    // Overall inbound packet rate (all senders, all types)
    // The drain window is ~1 second, so count ≈ rate.
    hp.packets_received_per_sec = Some(total_packets as f64);

    // Per-sender peer stats — this is the critical part for AQ.
    // The drain window is ~1 second, so packet counts ≈ per-second rates.
    for (sender_id, counters) in sender_counters {
        let mut ps = PbPeerStats::new();

        ps.can_listen = counters.audio_packets > 0;
        ps.can_see = counters.video_packets > 0;

        // Video stats — fps_received is the field senders use to decide
        // quality tiers. Since we drain every ~1s, video_packets ≈ fps.
        let mut vs = PbVideoStats::new();
        vs.fps_received = counters.video_packets as f64;
        vs.bitrate_kbps = counters.video_bytes * 8 / 1000; // bytes/s → kbps
        ps.video_stats = ::protobuf::MessageField::some(vs);

        // Stub NetEQ stats — bot does not use NetEQ but the field should
        // exist so the server doesn't treat it as missing.
        let mut ns = PbNetEqStats::new();
        ns.packets_per_sec = counters.audio_packets as f64;
        ps.neteq_stats = ::protobuf::MessageField::some(ns);

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
