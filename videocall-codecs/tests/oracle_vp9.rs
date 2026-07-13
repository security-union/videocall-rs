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

//! Oracle milestone tests for the pure-Rust VP9 encoder.
//!
//! Each test encodes synthetic frames with `videocall_codecs::vp9::Vp9Encoder`,
//! decodes the bitstream with libvpx (the oracle), and asserts on dimensions,
//! keyframe flags, PSNR, and bitrate. They land `#[ignore]`d; the ignore
//! attribute is removed by the PR that turns the corresponding milestone green.
//!
//! Requires `--features "libvpx test-utils"`.

use std::path::PathBuf;

use videocall_codecs::encoder::{Encodable, EncodedFrame, EncoderConfig};
use videocall_codecs::testing::i420::{gradient, moving_box};
use videocall_codecs::testing::ivf::IvfWriter;
use videocall_codecs::testing::oracle::{OracleDecoder, OracleFrame};
use videocall_codecs::testing::psnr::psnr_i420;
use videocall_codecs::vp9::Vp9Encoder;

// --- Thresholds (named constants; tighten as the encoder matures) ---

/// Minimum PSNR-Y (dB) for a decoded keyframe.
const PSNR_Y_KEYFRAME_MIN: f64 = 32.0;
/// Minimum average PSNR-Y (dB) across an inter sequence.
const PSNR_Y_SEQ_AVG_MIN: f64 = 28.0;
/// Minimum single-frame PSNR-Y (dB) across an inter sequence.
const PSNR_Y_SEQ_FRAME_MIN: f64 = 24.0;
/// Fractional bitrate tolerance band around the target (±50%).
const BITRATE_TOLERANCE: f64 = 0.5;

// --- Shared helpers ---

/// Standard test config: 150-frame keyframe interval, q 40..60, cpu_used 7.
fn config(width: u32, height: u32, framerate: u32, bitrate_kbps: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        framerate,
        bitrate_kbps,
        keyframe_interval: 150,
        min_quantizer: 40,
        max_quantizer: 60,
        cpu_used: 7,
    }
}

/// Encode a whole sequence, collecting every emitted frame. Panics on error.
fn encode_sequence(enc: &mut Vp9Encoder, frames: &[Vec<u8>]) -> Vec<EncodedFrame> {
    let mut out = Vec::with_capacity(frames.len());
    for (i, frame) in frames.iter().enumerate() {
        if let Some(ef) = enc.encode(i as i64, frame).expect("encode failed") {
            out.push(ef);
        }
    }
    out
}

/// Decode every compressed frame with the libvpx oracle, in order.
fn decode_all(frames: &[EncodedFrame]) -> Vec<OracleFrame> {
    let mut dec = OracleDecoder::new().expect("oracle init failed");
    let mut out = Vec::new();
    for f in frames {
        out.extend(dec.decode(&f.data).expect("oracle decode failed"));
    }
    out
}

/// Dump a stream to an IVF file in the temp dir for `vpxdec`/`ffprobe` triage.
fn dump_ivf(frames: &[EncodedFrame], width: u16, height: u16, fps: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "oracle_vp9_{}_{}.ivf",
        std::process::id(),
        frames.len()
    ));
    let file = std::fs::File::create(&path).expect("create ivf");
    let mut writer = IvfWriter::new(file, width, height, fps).expect("ivf header");
    for f in frames {
        writer
            .write_frame(&f.data, f.pts as u64)
            .expect("ivf frame");
    }
    writer.into_inner().expect("flush ivf");
    path
}

// --- Milestones ---

#[test]
#[ignore = "M1 pending: keyframe encode + oracle decode + PSNR"]
fn m1_keyframe_decodes_with_dims_and_psnr() {
    let (w, h, fps) = (640u32, 480u32, 30u32);
    let src = gradient(w, h, 0);

    let mut enc = Vp9Encoder::new(config(w, h, fps, 500)).expect("encoder init");
    let ef = enc
        .encode(0, &src)
        .expect("encode error")
        .expect("keyframe should be emitted immediately");
    assert!(ef.is_keyframe, "first frame must be a keyframe");

    let stream = vec![ef];
    let decoded = decode_all(&stream);
    if decoded.len() != 1 {
        let path = dump_ivf(&stream, w as u16, h as u16, fps);
        panic!(
            "expected exactly 1 decoded frame, got {} (ivf dumped to {})",
            decoded.len(),
            path.display()
        );
    }

    let d = &decoded[0];
    assert_eq!((d.width, d.height), (w, h), "decoded dimensions mismatch");

    let psnr = psnr_i420(&src, &d.i420, w, h);
    if psnr.y < PSNR_Y_KEYFRAME_MIN {
        let path = dump_ivf(&stream, w as u16, h as u16, fps);
        panic!(
            "keyframe PSNR-Y {:.2} dB < {:.2} dB (ivf dumped to {})",
            psnr.y,
            PSNR_Y_KEYFRAME_MIN,
            path.display()
        );
    }
}

#[test]
#[ignore = "M2 pending: inter sequence, one keyframe, PSNR band"]
fn m2_sequence_90_frames_kf150_one_key_rest_inter() {
    let (w, h, fps) = (640u32, 480u32, 30u32);
    let frames: Vec<Vec<u8>> = (0..90).map(|t| moving_box(w, h, t)).collect();

    let mut enc = Vp9Encoder::new(config(w, h, fps, 500)).expect("encoder init");
    let encoded = encode_sequence(&mut enc, &frames);

    assert_eq!(
        encoded.len(),
        frames.len(),
        "every input frame must produce exactly one output frame"
    );
    assert!(encoded[0].is_keyframe, "frame 0 must be a keyframe");
    assert!(
        encoded[1..].iter().all(|f| !f.is_keyframe),
        "frames 1.. must be delta frames (kf interval 150)"
    );

    let decoded = decode_all(&encoded);
    assert_eq!(decoded.len(), frames.len(), "all frames must decode");

    let ys: Vec<f64> = decoded
        .iter()
        .zip(frames.iter())
        .map(|(d, src)| psnr_i420(src, &d.i420, w, h).y)
        .collect();
    let avg = ys.iter().sum::<f64>() / ys.len() as f64;
    let min = ys.iter().copied().fold(f64::INFINITY, f64::min);

    assert!(
        avg >= PSNR_Y_SEQ_AVG_MIN,
        "avg PSNR-Y {avg:.2} dB < {PSNR_Y_SEQ_AVG_MIN:.2} dB"
    );
    assert!(
        min >= PSNR_Y_SEQ_FRAME_MIN,
        "min PSNR-Y {min:.2} dB < {PSNR_Y_SEQ_FRAME_MIN:.2} dB"
    );
}

#[test]
#[ignore = "M3 pending: rate control within tolerance"]
fn m3_bitrate_within_tolerance_over_150_frames() {
    let (w, h, fps, kbps) = (640u32, 480u32, 30u32, 500u32);
    let n = 150usize;
    let frames: Vec<Vec<u8>> = (0..n as u32).map(|t| moving_box(w, h, t)).collect();

    let mut enc = Vp9Encoder::new(config(w, h, fps, kbps)).expect("encoder init");
    let encoded = encode_sequence(&mut enc, &frames);

    let total_bytes: usize = encoded.iter().map(|f| f.data.len()).sum();
    let seconds = n as f64 / fps as f64;
    let measured_kbps = (total_bytes as f64 * 8.0 / 1000.0) / seconds;

    let lo = kbps as f64 * (1.0 - BITRATE_TOLERANCE); // 250
    let hi = kbps as f64 * (1.0 + BITRATE_TOLERANCE); // 750
    assert!(
        (lo..=hi).contains(&measured_kbps),
        "measured {measured_kbps:.1} kbps outside tolerance band {lo:.0}..={hi:.0}"
    );
}

#[test]
#[ignore = "M4 pending: runtime bitrate update takes effect"]
fn m4_update_bitrate_kbps_takes_effect() {
    let (w, h, fps) = (640u32, 480u32, 30u32);
    let mut enc = Vp9Encoder::new(config(w, h, fps, 300)).expect("encoder init");

    // Low-bitrate phase: 60 frames @300 kbps, summing bytes but excluding the
    // opening keyframe (which is bitrate-independent and would skew the ratio).
    let mut bytes_lo = 0usize;
    for t in 0..60u32 {
        let frame = moving_box(w, h, t);
        if let Some(ef) = enc.encode(t as i64, &frame).expect("encode failed") {
            if !(t == 0 && ef.is_keyframe) {
                bytes_lo += ef.data.len();
            }
        }
    }

    enc.update_bitrate_kbps(1200)
        .expect("bitrate update failed");

    // Skip 5 transient frames while the rate controller settles.
    for t in 60..65u32 {
        let frame = moving_box(w, h, t);
        let _ = enc.encode(t as i64, &frame).expect("encode failed");
    }

    // High-bitrate phase: 60 frames @1200 kbps.
    let mut bytes_hi = 0usize;
    for t in 65..125u32 {
        let frame = moving_box(w, h, t);
        if let Some(ef) = enc.encode(t as i64, &frame).expect("encode failed") {
            bytes_hi += ef.data.len();
        }
    }

    assert!(
        bytes_hi as f64 >= 2.0 * bytes_lo as f64,
        "high-bitrate bytes {bytes_hi} should be >= 2x low-bitrate bytes {bytes_lo}"
    );
}
