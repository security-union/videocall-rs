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

use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use web_time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("NetEQ Jitter Buffer Example");
    println!("============================");

    // Create NetEQ configuration
    let config = NetEqConfig {
        sample_rate: 16000,
        channels: 1,
        max_packets_in_buffer: 50,
        max_delay_ms: 500,
        min_delay_ms: 20,
        enable_fast_accelerate: true,
        enable_muted_state: false,
        enable_rtx_handling: false,
        for_test_no_time_stretching: false,
        ..Default::default()
    };

    // Create NetEQ instance
    let mut neteq = NetEq::new(config)?;

    println!(
        "Created NetEQ with target delay: {}ms",
        neteq.target_delay_ms()
    );

    // Simulate incoming audio packets with some jitter
    let mut sequence_number = 0u16;
    let mut timestamp = 0u32;
    let packet_duration_ms = 20; // 20ms packets
    let samples_per_packet = 320; // 20ms at 16kHz = 320 samples

    println!("\nSimulating incoming packets with jitter...");

    // Send packets with varying delays to simulate network jitter
    for i in 0..30 {
        // Create audio packet with sine wave content
        let packet = create_audio_packet(
            sequence_number,
            timestamp,
            samples_per_packet,
            16000,
            packet_duration_ms,
        );

        // Insert packet into NetEQ
        neteq.insert_packet(packet)?;

        // Simulate jitter by adding random delays
        let jitter_delay = match i % 5 {
            0 => 5,  // 5ms extra delay
            1 => 0,  // No extra delay
            2 => 15, // 15ms extra delay
            3 => 2,  // 2ms extra delay
            _ => 8,  // 8ms extra delay
        };

        if jitter_delay > 0 {
            std::thread::sleep(Duration::from_millis(jitter_delay));
        }

        sequence_number = sequence_number.wrapping_add(1);
        timestamp = timestamp.wrapping_add(samples_per_packet as u32);

        // Print buffer status every few packets
        if i % 5 == 0 {
            let stats = neteq.get_statistics();
            println!(
                "Packet {}: Buffer size: {}ms, Target delay: {}ms, Packets: {}",
                i, stats.current_buffer_size_ms, stats.target_delay_ms, stats.packet_count
            );
        }
    }

    println!("\nRetrieving audio frames...");

    // Retrieve audio frames (10ms each)
    let mut total_samples = 0;
    let mut expand_count = 0;

    for frame_num in 0..50 {
        let frame = neteq.get_audio()?;
        total_samples += frame.samples.len();

        // Count different frame types
        match frame.speech_type {
            neteq::neteq::SpeechType::Expand => expand_count += 1,
            neteq::neteq::SpeechType::Normal => {
                // Check if this was likely an accelerate operation
                // (simplified detection based on frame energy changes)
            }
            _ => {}
        }

        // Print status every 10 frames
        if frame_num % 10 == 0 {
            let stats = neteq.get_statistics();
            println!(
                "Frame {}: {}ms, Type: {:?}, Buffer: {}ms, VAD: {}",
                frame_num,
                frame.duration_ms(),
                frame.speech_type,
                stats.current_buffer_size_ms,
                frame.vad_activity
            );
        }
    }

    // Print final statistics
    let final_stats = neteq.get_statistics();
    println!("\nFinal Statistics:");
    println!("================");
    println!("Total samples processed: {}", total_samples);
    println!(
        "Packets received: {}",
        final_stats.lifetime.jitter_buffer_packets_received
    );
    println!(
        "Concealment events: {}",
        final_stats.lifetime.concealment_events
    );
    println!("Buffer flushes: {}", final_stats.lifetime.buffer_flushes);
    println!("Expand frames: {}", expand_count);
    println!(
        "Current buffer size: {}ms",
        final_stats.current_buffer_size_ms
    );
    println!("Target delay: {}ms", final_stats.target_delay_ms);

    println!("Network Statistics:");
    println!(
        "  Current buffer: {}ms",
        final_stats.network.current_buffer_size_ms
    );
    println!(
        "  Preferred buffer: {}ms",
        final_stats.network.preferred_buffer_size_ms
    );
    println!(
        "  Mean waiting time: {}ms",
        final_stats.network.mean_waiting_time_ms
    );
    println!("  Accelerate rate: {}", final_stats.network.accelerate_rate);
    println!("  Expand rate: {}", final_stats.network.expand_rate);

    println!("\nExample completed successfully!");

    Ok(())
}

/// Create a test audio packet with sine wave content
fn create_audio_packet(
    sequence_number: u16,
    timestamp: u32,
    samples: usize,
    sample_rate: u32,
    duration_ms: u32,
) -> AudioPacket {
    let header = RtpHeader::new(sequence_number, timestamp, 12345, 96, false);

    // Generate sine wave audio data
    let frequency = 440.0; // A4 note
    let mut payload = Vec::new();

    for i in 0..samples {
        let t = i as f32 / sample_rate as f32;
        let sample = (2.0 * std::f32::consts::PI * frequency * t).sin() * 0.1;
        payload.extend_from_slice(&sample.to_le_bytes());
    }

    AudioPacket::new(header, payload, sample_rate, 1, duration_ms)
}

/// Demonstrate adaptive behavior with burst loss
#[allow(dead_code)]
fn demonstrate_packet_loss() -> Result<(), Box<dyn std::error::Error>> {
    println!("\nDemonstrating packet loss handling...");

    let config = NetEqConfig::default();
    let mut neteq = NetEq::new(config)?;

    let mut sequence_number = 0u16;
    let mut timestamp = 0u32;

    // Send some normal packets
    for _i in 0..10 {
        let packet = create_audio_packet(sequence_number, timestamp, 160, 16000, 10);
        neteq.insert_packet(packet)?;

        sequence_number = sequence_number.wrapping_add(1);
        timestamp = timestamp.wrapping_add(160);
    }

    // Simulate burst loss (skip 5 packets)
    println!("Simulating burst loss of 5 packets...");
    sequence_number = sequence_number.wrapping_add(5);
    timestamp = timestamp.wrapping_add(5 * 160);

    // Continue with normal packets
    for _i in 0..10 {
        let packet = create_audio_packet(sequence_number, timestamp, 160, 16000, 10);
        neteq.insert_packet(packet)?;

        sequence_number = sequence_number.wrapping_add(1);
        timestamp = timestamp.wrapping_add(160);
    }

    // Retrieve frames and observe concealment
    for frame_num in 0..30 {
        let frame = neteq.get_audio()?;

        if frame_num % 5 == 0 {
            println!(
                "Frame {}: Type: {:?}, VAD: {}",
                frame_num, frame.speech_type, frame.vad_activity
            );
        }
    }

    let stats = neteq.get_statistics();
    println!(
        "Concealment events after loss: {}",
        stats.lifetime.concealment_events
    );

    Ok(())
}
