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

//! Real RTT probe for passthrough bots.
//!
//! Sends an RTT probe packet every 2 seconds (matching the browser's cadence)
//! and measures the actual round-trip time when the relay echoes it back.
//!
//! The relay recognizes packets with `media_type == RTT` and echoes the entire
//! `PacketWrapper` back verbatim to the sender (see
//! `actix-api/src/actors/session_logic.rs:403` — `InboundAction::Echo`).
//!
//! The probe embeds a monotonic timestamp (milliseconds since an arbitrary
//! epoch) in the `MediaPacket.timestamp` field. When the echo returns, the
//! current time is compared against the embedded timestamp to compute RTT.
//!
//! The measured RTT is exposed via a shared `Arc<AtomicU64>` (storing f64 bits)
//! which the health reporter reads for `active_server_rtt_ms`.

use protobuf::Message;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tracing::{debug, info, warn};

use crate::transport::{MediaTypeLabel, OutboundFrame};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Shared RTT state between the probe sender and the inbound echo handler.
///
/// The sender records the `Instant` when a probe was sent; the inbound path
/// calls `record_echo` with the echoed timestamp to compute RTT.
pub struct RttProbeState {
    /// Monotonic reference point. All timestamps are relative to this epoch
    /// so we can convert between `Instant` and the `f64` millis on the wire.
    epoch: Instant,
    /// Current smoothed RTT in milliseconds, stored as f64 bits.
    /// Updated via exponential moving average (alpha = 0.3).
    pub rtt_ms: Arc<AtomicU64>,
}

impl Default for RttProbeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RttProbeState {
    /// Create a new RTT probe state with a fresh epoch.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            rtt_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the current monotonic time as milliseconds relative to our epoch.
    fn now_ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// Called by the inbound path when an echoed RTT packet is received.
    /// `sent_timestamp_ms` is the value from the echoed `MediaPacket.timestamp`.
    pub fn record_echo(&self, sent_timestamp_ms: f64) {
        let now = self.now_ms();
        let rtt = now - sent_timestamp_ms;
        if rtt < 0.0 || !rtt.is_finite() {
            debug!("RTT probe: ignoring invalid RTT value: {:.1}ms", rtt);
            return;
        }

        // Exponential moving average with alpha = 0.3
        let prev_bits = self.rtt_ms.load(Ordering::Relaxed);
        let prev = f64::from_bits(prev_bits);
        let smoothed = if prev == 0.0 {
            rtt
        } else {
            prev * 0.7 + rtt * 0.3
        };
        self.rtt_ms.store(smoothed.to_bits(), Ordering::Relaxed);
        debug!(
            "RTT probe: measured={:.1}ms smoothed={:.1}ms",
            rtt, smoothed
        );
    }

    /// Returns the shared atomic holding the current smoothed RTT (f64 bits).
    pub fn rtt_atomic(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.rtt_ms)
    }
}

/// Spawn the RTT probe sender task.
///
/// Sends one RTT probe every 2 seconds through the shared `packet_tx` channel.
/// Returns the shared `RttProbeState` so the inbound path can call `record_echo`.
///
/// The task runs until `quit` is set to true.
pub fn spawn_rtt_probe(
    user_id: String,
    packet_tx: Sender<OutboundFrame>,
    quit: Arc<std::sync::atomic::AtomicBool>,
) -> Arc<RttProbeState> {
    let state = Arc::new(RttProbeState::new());
    let state_clone = Arc::clone(&state);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        // Skip the first immediate tick — give the transport time to connect.
        interval.tick().await;

        info!("[{}] RTT probe task started (2s interval)", user_id);

        loop {
            interval.tick().await;

            if quit.load(Ordering::Relaxed) {
                break;
            }

            let timestamp_ms = state_clone.now_ms();

            let bytes = match build_rtt_probe(&user_id, timestamp_ms) {
                Ok(b) => b,
                Err(e) => {
                    warn!("[{}] Failed to build RTT probe: {}", user_id, e);
                    continue;
                }
            };

            let frame = OutboundFrame::new(MediaTypeLabel::Other, bytes);
            if packet_tx.try_send(frame).is_err() {
                debug!("[{}] RTT probe dropped (channel full)", user_id);
            }
        }

        info!("[{}] RTT probe task stopped", user_id);
    });

    state
}

/// Build a serialized RTT probe `PacketWrapper`.
fn build_rtt_probe(user_id: &str, timestamp_ms: f64) -> anyhow::Result<Vec<u8>> {
    let media_packet = MediaPacket {
        media_type: MediaType::RTT.into(),
        user_id: user_id.as_bytes().to_vec(),
        timestamp: timestamp_ms,
        ..Default::default()
    };
    let media_data = media_packet.write_to_bytes()?;

    let wrapper = PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        user_id: user_id.as_bytes().to_vec(),
        data: media_data,
        ..Default::default()
    };

    Ok(wrapper.write_to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_state_initial_is_zero() {
        let state = RttProbeState::new();
        let bits = state.rtt_ms.load(Ordering::Relaxed);
        assert_eq!(f64::from_bits(bits), 0.0);
    }

    #[test]
    fn rtt_state_records_first_echo() {
        let state = RttProbeState::new();
        // Simulate: sent at epoch+0, received at epoch+50ms
        std::thread::sleep(Duration::from_millis(50));
        state.record_echo(0.0);
        let bits = state.rtt_ms.load(Ordering::Relaxed);
        let rtt = f64::from_bits(bits);
        // Should be approximately 50ms (first sample, no smoothing)
        assert!(rtt > 30.0 && rtt < 200.0, "unexpected RTT: {}", rtt);
    }

    #[test]
    fn rtt_state_smooths_subsequent_echoes() {
        let state = RttProbeState::new();
        // Manually set initial RTT to 100ms
        state.rtt_ms.store(100.0_f64.to_bits(), Ordering::Relaxed);

        // Record an echo that suggests 50ms RTT
        // epoch.elapsed() will be small, so we use a negative timestamp
        // to simulate the delta. Instead, let's call record_echo with a
        // timestamp that's now_ms() - 50.
        let fake_sent = state.now_ms() - 50.0;
        state.record_echo(fake_sent);

        let bits = state.rtt_ms.load(Ordering::Relaxed);
        let rtt = f64::from_bits(bits);
        // EMA: 100 * 0.7 + 50 * 0.3 = 85
        assert!(
            (rtt - 85.0).abs() < 10.0,
            "expected ~85ms smoothed RTT, got {}",
            rtt
        );
    }

    #[test]
    fn rtt_ignores_negative_values() {
        let state = RttProbeState::new();
        state.rtt_ms.store(50.0_f64.to_bits(), Ordering::Relaxed);
        // Send a future timestamp — would produce negative RTT
        let future_ts = state.now_ms() + 1000.0;
        state.record_echo(future_ts);
        // Should remain unchanged
        let bits = state.rtt_ms.load(Ordering::Relaxed);
        let rtt = f64::from_bits(bits);
        assert!(
            (rtt - 50.0).abs() < 1.0,
            "RTT should be unchanged at ~50ms, got {}",
            rtt
        );
    }

    #[test]
    fn build_rtt_probe_produces_valid_packet() {
        let bytes = build_rtt_probe("test-bot", 12345.678).expect("build");
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).expect("parse wrapper");
        assert_eq!(wrapper.packet_type.enum_value(), Ok(PacketType::MEDIA));
        assert_eq!(wrapper.user_id, b"test-bot");

        let media = MediaPacket::parse_from_bytes(&wrapper.data).expect("parse media");
        assert_eq!(media.media_type.enum_value(), Ok(MediaType::RTT));
        assert_eq!(media.user_id, b"test-bot");
        assert!((media.timestamp - 12345.678).abs() < 0.001);
    }
}
