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

//! Realtime performance gate for the pure-Rust VP9 encoder.
//!
//! Pure Rust, no libvpx. Ignored by default; run explicitly with:
//! `cargo test -p videocall-codecs --release --features test-utils --test perf_gate -- --ignored`

use std::time::Instant;

use videocall_codecs::encoder::{Encodable, EncoderConfig};
use videocall_codecs::testing::i420::moving_box;
use videocall_codecs::vp9::Vp9Encoder;

/// Per-frame encode budget for 30 fps realtime.
const FRAME_BUDGET_MS: f64 = 33.0;

#[test]
#[ignore = "perf gate: run with cargo test --release -- --ignored"]
fn perf_median_encode_frame_under_33ms_640x480() {
    let (w, h) = (640u32, 480u32);
    let cfg = EncoderConfig {
        width: w,
        height: h,
        ..Default::default()
    };
    let mut enc = Vp9Encoder::new(cfg).expect("encoder init");

    // Warm up (allocations, caches) without timing.
    for t in 0..10u32 {
        let frame = moving_box(w, h, t);
        let _ = enc.encode(t as i64, &frame);
    }

    let mut times_ms = Vec::with_capacity(300);
    for t in 10..310u32 {
        let frame = moving_box(w, h, t);
        let start = Instant::now();
        let _ = enc.encode(t as i64, &frame);
        times_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| times_ms[(((times_ms.len() - 1) as f64) * p) as usize];
    let (p50, p90, p99) = (pct(0.50), pct(0.90), pct(0.99));
    println!("encode latency ms: p50={p50:.3} p90={p90:.3} p99={p99:.3}");

    assert!(
        p50 < FRAME_BUDGET_MS,
        "median encode {p50:.3} ms exceeds {FRAME_BUDGET_MS} ms realtime budget"
    );
}
