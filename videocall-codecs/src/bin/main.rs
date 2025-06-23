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

use videocall_codecs::{
    decoder::{Decodable, Decoder, VideoCodec},
    frame::{FrameType, VideoFrame},
    jitter_buffer::JitterBuffer,
};

use rand::seq::SliceRandom;
use rand::thread_rng;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    println!("--- Video Decoder Jitter Buffer Simulation ---");

    let on_decoded_frame = |frame: videocall_codecs::decoder::DecodedFrame| {
        println!(
            "[MAIN_THREAD] Received decoded frame: {}",
            frame.sequence_number
        );
    };

    // The simulation now requests the lightweight Mock codec.
    let decoder = Decoder::new(VideoCodec::Mock, Box::new(on_decoded_frame));
    let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::<
        videocall_codecs::decoder::DecodedFrame,
    >::new(Box::new(decoder))));

    // --- Network Simulation Loop ---
    let network_thread = std::thread::spawn(move || {
        let mut sequence_number: u64 = 0;
        loop {
            let mut batch = Vec::new();
            batch.push(VideoFrame {
                sequence_number,
                frame_type: FrameType::KeyFrame,
                data: vec![0; 1000],
                timestamp: 0.0,
            });
            sequence_number += 1;

            for _ in 0..15 {
                batch.push(VideoFrame {
                    sequence_number,
                    frame_type: FrameType::DeltaFrame,
                    data: vec![0; 200],
                    timestamp: 0.0,
                });
                sequence_number += 1;
            }

            batch.retain(|frame| {
                if frame.frame_type == FrameType::KeyFrame {
                    return true;
                }
                rand::random::<f32>() > 0.1
            });

            batch.shuffle(&mut thread_rng());

            for frame in batch {
                let arrival_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis();

                let mut jb = jitter_buffer.lock().unwrap();
                jb.insert_frame(frame, arrival_time);
                drop(jb);

                std::thread::sleep(Duration::from_millis(20));
            }

            let jb = jitter_buffer.lock().unwrap();
            println!(
                "\n[STATS] Jitter Estimate: {:.2}ms | Target Playout Delay: {:.2}ms\n",
                jb.get_jitter_estimate_ms(),
                jb.get_target_playout_delay_ms()
            );
            drop(jb);

            std::thread::sleep(Duration::from_secs(1));
        }
    });

    network_thread.join().unwrap();
}
