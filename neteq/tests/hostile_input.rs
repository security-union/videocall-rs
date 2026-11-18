//! Hostile-input robustness: remote peers control the Opus payload, the
//! sequence number and (through the receiver's timestamp mapper) the RTP
//! timestamp. None of that may panic NetEq, in debug builds included, because
//! a panic in the browser worker kills that peer's audio.
//!
//! This is a deterministic random walk, not a parity test: it only checks
//! that `insert_packet` and `get_audio` return.

#![cfg(feature = "ropus")]

use std::sync::Arc;

use neteq::api::{AudioFrame, Config, NetEq, RtpHeader, RtpPacketInfo, SdpAudioFormat};
use neteq::api::{Clock, SimulatedClock};

const OPUS_PT: u8 = 111;

/// xorshift64*, so the walk is reproducible without a `rand` dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn opus_neteq(channels: usize, clock: &SimulatedClock) -> NetEq {
    let config = Config {
        sample_rate_hz: 48_000,
        enable_muted_state: true,
        ..Config::default()
    };
    let mut neteq = NetEq::builder(config)
        .clock(Arc::new(clock.clone()) as Arc<dyn Clock>)
        .build();
    let mut format = SdpAudioFormat::new("opus", 48_000, 2);
    if channels == 2 {
        format
            .parameters
            .insert("stereo".to_string(), "1".to_string());
    }
    assert!(neteq.register_payload_type(i32::from(OPUS_PT), &format));
    neteq
}

/// A payload that is sometimes empty, sometimes a lone TOC byte, sometimes
/// a valid-looking TOC with garbage frames, sometimes pure garbage.
fn hostile_payload(rng: &mut Rng) -> Vec<u8> {
    match rng.below(8) {
        0 => Vec::new(),
        1 => vec![rng.next() as u8],
        2 => {
            // Code 3 packet with a random frame count and padding flags.
            let mut payload = vec![(rng.next() as u8) | 0x03, rng.next() as u8];
            let len = rng.below(64) as usize;
            payload.extend(rng.bytes(len));
            payload
        }
        3 => {
            let len = rng.below(2_000) as usize;
            rng.bytes(len)
        }
        _ => {
            let len = 1 + rng.below(200) as usize;
            let mut payload = rng.bytes(len);
            // Keep code 0 (one frame) most of the time so CELT/SILK decode.
            payload[0] &= 0xFC;
            payload
        }
    }
}

/// Runs one walk; returns the samples NetEq decoded, so the tests can check
/// the walk reached the decoder instead of bouncing off `insert_packet`.
fn run_walk(seed: u64, channels: usize, steps: usize) -> u64 {
    let mut rng = Rng(seed.max(1));
    let clock = SimulatedClock::new(1_000_000);
    let mut neteq = opus_neteq(channels, &clock);
    let mut frame = AudioFrame::new();
    let mut seq: u16 = rng.next() as u16;
    let mut ts: u32 = rng.next() as u32;

    for _ in 0..steps {
        let packets = rng.below(4);
        for _ in 0..packets {
            // Mostly a plausible stream, with hostile jumps mixed in.
            match rng.below(16) {
                0 => seq = rng.next() as u16,
                1 => ts = rng.next() as u32,
                2 => ts = ts.wrapping_sub(rng.below(1 << 20) as u32),
                3 => seq = seq.wrapping_sub(rng.below(300) as u16),
                _ => {
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(960);
                }
            }
            let header = RtpHeader {
                marker_bit: rng.below(2) == 0,
                payload_type: OPUS_PT,
                sequence_number: seq,
                timestamp: ts,
                ssrc: 0x1234_5678,
            };
            let payload = hostile_payload(&mut rng);
            let _ = neteq.insert_packet(
                &header,
                &payload,
                RtpPacketInfo::new(&header, Some(clock.current_time_us())),
            );
        }
        let _ = neteq.get_audio(&mut frame, None);
        clock.advance_time_ms(10);
        if rng.below(500) == 0 {
            neteq.flush_buffers();
        }
    }
    // The stats paths do arithmetic on the counters the walk drove.
    let _ = neteq.network_statistics();
    let operations = neteq.operations_and_state();
    let lifetime = neteq.lifetime_statistics();
    eprintln!(
        "seed {seed:#x}: received {} concealed {} accelerate {} preemptive {} flushes {}",
        lifetime.total_samples_received,
        lifetime.concealed_samples,
        operations.accelerate_samples,
        operations.preemptive_samples,
        operations.packet_buffer_flushes,
    );
    lifetime.total_samples_received
}

/// Extra seeds for a longer local soak: `NETEQ_HOSTILE_SEEDS=500`.
fn seed_count(default: u64) -> u64 {
    std::env::var("NETEQ_HOSTILE_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
fn hostile_opus_stream_mono_does_not_panic() {
    let decoded: u64 = (0..seed_count(12))
        .map(|seed| run_walk(0x9E37_79B9_7F4A_7C15 ^ seed, 1, 3_000))
        .sum();
    assert!(decoded > 0, "the walk never reached the decoder");
}

#[test]
fn hostile_opus_stream_stereo_does_not_panic() {
    let decoded: u64 = (0..seed_count(6))
        .map(|seed| run_walk(0xD1B5_4A32_D192_ED03 ^ seed, 2, 3_000))
        .sum();
    assert!(decoded > 0, "the walk never reached the decoder");
}
