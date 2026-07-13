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

//! Robustness sweeps for the pure-Rust VP9 encoder: hostile geometries,
//! quantizer extremes, and runtime state changes must never panic and must
//! always emit a plausible frame. Pure Rust (no libvpx), so this runs in the
//! default C-free CI alongside the unit tests; the oracle tests
//! (`oracle_vp9.rs`) cover bit-exactness at the well-behaved resolutions.

use videocall_codecs::encoder::{Encodable, EncoderConfig};
use videocall_codecs::vp9::Vp9Encoder;

/// Tight-packed I420 of `w`x`h` with a mild moving gradient at time `t`.
fn i420(w: u32, h: u32, t: u32) -> Vec<u8> {
    let (yw, yh) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
    let mut v = Vec::with_capacity(yw * yh + 2 * cw * ch);
    for r in 0..yh {
        for c in 0..yw {
            v.push(((r + c + t as usize * 3) & 0xff) as u8);
        }
    }
    v.extend(std::iter::repeat_n(128u8, 2 * cw * ch));
    v
}

fn base(w: u32, h: u32) -> EncoderConfig {
    EncoderConfig {
        width: w,
        height: h,
        framerate: 30,
        bitrate_kbps: 500,
        keyframe_interval: 150,
        min_quantizer: 40,
        max_quantizer: 60,
        cpu_used: 7,
    }
}

/// Encode a short sequence and assert every emitted frame is structurally
/// plausible (non-empty, frame marker present, keyframe cadence sane).
fn drive(cfg: EncoderConfig, frames: u32) {
    let (w, h) = (cfg.width, cfg.height);
    let mut enc = Vp9Encoder::new(cfg).expect("encoder init");
    for t in 0..frames {
        let ef = enc
            .encode(t as i64, &i420(w, h, t))
            .unwrap_or_else(|e| panic!("{w}x{h} t{t}: encode error: {e}"))
            .unwrap_or_else(|| panic!("{w}x{h} t{t}: no frame emitted"));
        assert!(!ef.data.is_empty(), "{w}x{h} t{t}: empty frame");
        assert_eq!(ef.data[0] >> 6, 0b10, "{w}x{h} t{t}: frame marker missing");
        assert_eq!(ef.is_keyframe, t == 0, "{w}x{h} t{t}: keyframe cadence");
    }
}

#[test]
fn tiny_and_odd_dimensions_do_not_panic() {
    // Even, non-zero dimensions the CLI would accept, from the smallest legal
    // frame up through odd mi grids and non-superblock-aligned sizes.
    for &(w, h) in &[
        (2u32, 2u32),
        (2, 480),
        (640, 2),
        (8, 8),
        (16, 16),
        (18, 18),
        (66, 66),   // one px past a superblock: partial SB + odd mi
        (328, 248), // odd mi_cols (41) and mi_rows (31): forced 16x16 edge split
        (330, 250),
    ] {
        drive(base(w, h), 4);
    }
}

#[test]
fn quantizer_extremes_do_not_panic() {
    // Pinned-low, pinned-high, inverted window, and out-of-range quantizers.
    for &(minq, maxq) in &[(0u32, 0u32), (63, 63), (60, 40), (0, 63), (1000, 5)] {
        let mut cfg = base(160, 120);
        cfg.min_quantizer = minq;
        cfg.max_quantizer = maxq;
        drive(cfg, 6);
    }
}

#[test]
fn bitrate_extremes_and_updates_do_not_panic() {
    for &kbps in &[1u32, 10, 100_000, 1_000_000] {
        let mut cfg = base(160, 120);
        cfg.bitrate_kbps = kbps;
        drive(cfg, 4);
    }

    // Runtime bitrate swings across the extremes on a live encoder.
    let (w, h) = (160u32, 120u32);
    let mut enc = Vp9Encoder::new(base(w, h)).expect("encoder init");
    for (t, &kbps) in [50u32, 5_000, 20, 500_000, 1].iter().enumerate() {
        enc.update_bitrate_kbps(kbps).expect("bitrate update");
        let ef = enc
            .encode(t as i64, &i420(w, h, t as u32))
            .expect("encode")
            .expect("frame");
        assert!(!ef.data.is_empty());
    }
}

#[test]
fn cpu_used_presets_all_encode() {
    // Every preset (including out-of-range) must encode both a keyframe and an
    // inter frame at an odd mi grid without panicking.
    for cpu in [0u8, 4, 5, 6, 7, 8, 200] {
        let mut cfg = base(328, 248);
        cfg.cpu_used = cpu;
        drive(cfg, 3);
    }
}

#[test]
fn keyframe_interval_one_and_forced_keyframes() {
    // kf interval 1 → every frame a keyframe.
    let (w, h) = (160u32, 120u32);
    let mut cfg = base(w, h);
    cfg.keyframe_interval = 1;
    let mut enc = Vp9Encoder::new(cfg).expect("encoder init");
    for t in 0..4 {
        let ef = enc.encode(t, &i420(w, h, t as u32)).unwrap().unwrap();
        assert!(
            ef.is_keyframe,
            "kf interval 1: frame {t} must be a keyframe"
        );
    }

    // Forced keyframe mid-stream refreshes exactly one frame.
    let mut enc = Vp9Encoder::new(base(w, h)).expect("encoder init");
    assert!(enc.encode(0, &i420(w, h, 0)).unwrap().unwrap().is_keyframe);
    assert!(!enc.encode(1, &i420(w, h, 1)).unwrap().unwrap().is_keyframe);
    enc.force_keyframe();
    assert!(
        enc.encode(2, &i420(w, h, 2)).unwrap().unwrap().is_keyframe,
        "forced frame must be a keyframe"
    );
    assert!(
        !enc.encode(3, &i420(w, h, 3)).unwrap().unwrap().is_keyframe,
        "force flag must clear after one frame"
    );
}
