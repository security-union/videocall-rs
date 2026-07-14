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

//! Keyframe decode-throughput benchmarks: pure-Rust VP9 vs libvpx VP9.
//!
//! Requires `--features "libvpx test-utils"`. The encode step that produces the
//! byte buffers is done once during setup, OUTSIDE the timed region, so only the
//! decode is measured. Both arms decode the identical set of encoded keyframes;
//! iterating over a set (rather than one buffer) keeps the workload varied.
//!
//! M0 of the pure-Rust decoder is keyframe-only, so the source encoder is driven
//! with `keyframe_interval = 1` (every emitted frame is an intra keyframe).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use videocall_codecs::encoder::{Encodable, EncoderConfig};
use videocall_codecs::testing::i420::moving_box;
use videocall_codecs::testing::oracle::OracleDecoder;
use videocall_codecs::vp9::dec::decode_keyframe;
use videocall_codecs::vp9::Vp9Encoder as PureVp9Encoder;

/// Number of distinct keyframes to encode up front and cycle through while
/// decoding, so neither arm is decoding one identical buffer repeatedly.
const NUM_FRAMES: u32 = 30;

fn bench_decoders(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp9_decode");
    group.throughput(Throughput::Elements(1));

    // 640x480 → 2 tile columns; 1280x720 → 4 tile columns. Decode is cheap enough
    // to include 720p without slowing the bench setup meaningfully.
    for &(w, h) in &[(640u32, 480u32), (1280u32, 720u32)] {
        // --- Setup (untimed): encode a set of keyframes into byte buffers. ---
        let cfg = EncoderConfig {
            width: w,
            height: h,
            keyframe_interval: 1, // every frame is an intra keyframe
            ..Default::default()
        };
        let mut enc = PureVp9Encoder::new(cfg).expect("pure encoder init");
        let encoded: Vec<Vec<u8>> = (0..NUM_FRAMES)
            .map(|t| {
                let i420 = moving_box(w, h, t);
                let frame = enc
                    .encode(t as i64, &i420)
                    .expect("encode failed")
                    .expect("encoder emitted a frame");
                assert!(frame.is_keyframe, "expected a keyframe with interval 1");
                frame.data
            })
            .collect();

        // --- Arm 1: pure-Rust decode. ---
        group.bench_function(format!("pure_rust_vp9_decode/{w}x{h}"), |b| {
            let mut i = 0usize;
            b.iter(|| {
                let bytes = &encoded[i % encoded.len()];
                let frame = decode_keyframe(black_box(bytes)).expect("pure-Rust decode");
                black_box(frame);
                i += 1;
            });
        });

        // --- Arm 2: libvpx decode of the SAME buffers, for comparison. ---
        group.bench_function(format!("libvpx_vp9_decode/{w}x{h}"), |b| {
            let mut dec = OracleDecoder::new().expect("oracle init");
            let mut i = 0usize;
            b.iter(|| {
                let bytes = &encoded[i % encoded.len()];
                let frames = dec.decode(black_box(bytes)).expect("libvpx decode");
                black_box(frames);
                i += 1;
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_decoders);
criterion_main!(benches);
