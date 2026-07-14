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

//! Encode-throughput benchmarks: pure-Rust VP9 vs legacy libvpx VP9.
//!
//! Requires `--features "libvpx test-utils"`. While the pure-Rust encoder is
//! unimplemented its bench still compiles and runs (the error is black-boxed),
//! so the harness is exercised from day one.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use videocall_codecs::encoder::libvpx::Vp9Encoder as LibvpxVp9Encoder;
use videocall_codecs::encoder::{Encodable, EncoderConfig};
use videocall_codecs::testing::i420::moving_box;
use videocall_codecs::vp9::Vp9Encoder as PureVp9Encoder;

fn bench_encoders(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp9_encode");
    group.throughput(Throughput::Elements(1));

    // 640x480 → 2 tile columns; 1280x720 → 4 tile columns (the tiling ceiling
    // rises with width, so the parallel speedup should grow at 720p).
    for &(w, h) in &[(640u32, 480u32), (1280u32, 720u32)] {
        let frames: Vec<Vec<u8>> = (0..90u32).map(|t| moving_box(w, h, t)).collect();
        let cfg = EncoderConfig {
            width: w,
            height: h,
            ..Default::default()
        };

        group.bench_function(format!("pure_rust_vp9/{w}x{h}"), |b| {
            let mut enc = PureVp9Encoder::new(cfg).expect("pure encoder init");
            let mut i = 0usize;
            b.iter(|| {
                let frame = &frames[i % frames.len()];
                match enc.encode(i as i64, frame) {
                    Ok(out) => {
                        black_box(out);
                    }
                    Err(e) => {
                        black_box(e);
                    }
                }
                i += 1;
            });
        });

        group.bench_function(format!("libvpx_vp9/{w}x{h}"), |b| {
            // Disambiguate from the legacy inherent `new`/`encode` via UFCS.
            let mut enc = <LibvpxVp9Encoder as Encodable>::new(cfg).expect("libvpx encoder init");
            let mut i = 0usize;
            b.iter(|| {
                let frame = &frames[i % frames.len()];
                let out = Encodable::encode(&mut enc, i as i64, frame).expect("libvpx encode");
                black_box(out);
                i += 1;
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_encoders);
criterion_main!(benches);
