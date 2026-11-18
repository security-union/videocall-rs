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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Feed NetEq a jittery stream of 20 ms PCM16B (L16) packets on a simulated
//! clock, pull 10 ms frames, and print what NetEq did.
//!
//! Run with `cargo run -p neteq --example basic_usage`.

use std::collections::BTreeMap;
use std::sync::Arc;

use neteq::api::{Clock, SimulatedClock};
use neteq::codecs::pcm16b::pcm16b_encode;
use neteq::{
    AudioFrame, Config, NetEq, Operation, RtpHeader, RtpPacketInfo, SdpAudioFormat,
    TelemetryCollector,
};

const SAMPLE_RATE_HZ: i32 = 16_000;
const PAYLOAD_TYPE: u8 = 96;
const PACKET_MS: i64 = 20;
const SAMPLES_PER_PACKET: usize = (SAMPLE_RATE_HZ as usize / 1000) * PACKET_MS as usize;

/// One 20 ms packet of a 440 Hz tone, PCM16B encoded.
fn tone_packet(first_sample: usize) -> Vec<u8> {
    let speech: Vec<i16> = (first_sample..first_sample + SAMPLES_PER_PACKET)
        .map(|n| {
            let t = n as f64 / f64::from(SAMPLE_RATE_HZ);
            (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
        })
        .collect();
    let mut payload = vec![0u8; speech.len() * 2];
    let written = pcm16b_encode(&speech, &mut payload);
    payload.truncate(written);
    payload
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // A simulated clock makes the run deterministic: time only moves when we
    // say so, 10 ms per GetAudio, like an audio device would pull.
    let clock = SimulatedClock::new(0);
    let mut neteq = NetEq::builder(Config::default())
        .clock(Arc::new(clock.clone()))
        .build();
    let codecs = BTreeMap::from([(
        i32::from(PAYLOAD_TYPE),
        SdpAudioFormat::new("L16", SAMPLE_RATE_HZ, 1),
    )]);
    neteq.set_codecs(&codecs);

    // Packet n is sent at n * 20 ms and arrives after 0..60 ms of jitter.
    let jitter_ms = [0, 35, 10, 60, 5, 20, 0, 45, 15, 30];
    let packet_count = 100usize;
    let mut arrivals: Vec<(i64, usize)> = (0..packet_count)
        .map(|n| {
            let send_ms = n as i64 * PACKET_MS;
            (send_ms + jitter_ms[n % jitter_ms.len()], n)
        })
        .collect();
    arrivals.sort();

    let mut collector = TelemetryCollector::new();
    let mut frame = AudioFrame::new();
    let mut next_arrival = 0;
    let mut operations: BTreeMap<String, u32> = BTreeMap::new();
    let end_ms = packet_count as i64 * PACKET_MS + 200;

    for now_ms in (0..end_ms).step_by(10) {
        // Insert everything that has arrived by now, then pull 10 ms.
        while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= now_ms {
            let n = arrivals[next_arrival].1;
            let header = RtpHeader {
                marker_bit: false,
                payload_type: PAYLOAD_TYPE,
                sequence_number: n as u16,
                timestamp: (n * SAMPLES_PER_PACKET) as u32,
                ssrc: 0x1234_5678,
            };
            let info = RtpPacketInfo::new(&header, Some(clock.current_time_us()));
            neteq.insert_packet(&header, &tone_packet(n * SAMPLES_PER_PACKET), info)?;
            collector.on_packet_inserted(header.sequence_number, now_ms as u64);
            next_arrival += 1;
        }

        neteq.get_audio(&mut frame, None)?;
        let operation = neteq.last_operation();
        collector.on_get_audio(operation, now_ms as u64);
        *operations.entry(format!("{operation:?}")).or_default() += 1;
        clock.advance_time_ms(10);
    }

    let stats = neteq::NetEqStats::from_neteq(&neteq, &collector);
    let lifetime = &stats.lifetime;
    println!("NetEq basic usage: {packet_count} packets of 20 ms L16 with up to 60 ms jitter");
    println!("  target delay        {} ms", neteq.target_delay_ms());
    println!("  buffered now        {} ms", stats.current_buffer_size_ms);
    println!(
        "  concealed samples   {} of {}",
        lifetime.concealed_samples, lifetime.total_samples_received
    );
    println!(
        "  reordered packets   {} of {}",
        stats.network.reordered_packets, stats.network.total_packets_received
    );
    println!("  operations per 10 ms tick:");
    for (operation, count) in &operations {
        println!("    {operation:<18} {count}");
    }
    assert!(operations.contains_key(&format!("{:?}", Operation::Normal)));
    Ok(())
}
