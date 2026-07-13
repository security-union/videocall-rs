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
use videocall_codecs::testing::i420::{busy, gradient, moving_box};
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

/// Informational: report the encoded size and PSNR of the M1 gradient keyframe
/// at the current fixed quantizer. Not a pass/fail gate.
#[test]
#[ignore = "informational: prints size + PSNR"]
fn m1_report_size_and_psnr() {
    let (w, h) = (640u32, 480u32);
    let src = gradient(w, h, 0);
    let mut enc = Vp9Encoder::new(config(w, h, 30, 500)).expect("encoder init");
    let ef = enc.encode(0, &src).expect("encode").expect("keyframe");
    let decoded = decode_all(std::slice::from_ref(&ef));
    let psnr = psnr_i420(&src, &decoded[0].i420, w, h);
    eprintln!(
        "M1 gradient {w}x{h}: {} bytes, PSNR-Y {:.2} dB, PSNR-U {:.2} dB, PSNR-V {:.2} dB",
        ef.data.len(),
        psnr.y,
        psnr.u,
        psnr.v
    );
}

/// Bit-exact reconstruction: the encoder's own reconstruction buffer must match,
/// sample-for-sample, what the libvpx oracle decodes from the same bitstream.
/// Any drift (prediction, transform rounding, dequant, context bookkeeping)
/// shows up here at the first bad frame. Odd sizes exercise mi padding and the
/// partial bottom superblock row.
#[test]
fn m1b_recon_matches_oracle_decode() {
    for &(w, h) in &[(176u32, 144u32), (640, 480), (636, 476)] {
        for (label, src) in [
            ("gradient", gradient(w, h, 0)),
            ("moving_box", moving_box(w, h, 3)),
        ] {
            let mut enc = Vp9Encoder::new(config(w, h, 30, 500)).expect("encoder init");
            let ef = enc
                .encode(0, &src)
                .expect("encode error")
                .expect("keyframe should be emitted");
            let recon = enc
                .last_reconstruction_i420()
                .expect("reconstruction available");

            let mut dec = OracleDecoder::new().expect("oracle init failed");
            let decoded = dec.decode(&ef.data).expect("oracle decode failed");
            assert_eq!(decoded.len(), 1, "{label} {w}x{h}: expected one frame");
            let d = &decoded[0];
            assert_eq!((d.width, d.height), (w, h), "{label} {w}x{h}: dims");
            assert_eq!(
                d.i420.len(),
                recon.len(),
                "{label} {w}x{h}: buffer length mismatch"
            );

            if d.i420 != recon {
                let first = d
                    .i420
                    .iter()
                    .zip(recon.iter())
                    .position(|(a, b)| a != b)
                    .unwrap();
                let diffs = d
                    .i420
                    .iter()
                    .zip(recon.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                panic!(
                    "{label} {w}x{h}: recon drift — {diffs} samples differ, first at byte {first} \
                     (oracle {} vs recon {})",
                    d.i420[first], recon[first]
                );
            }
        }
    }
}

/// Bit-exact inter drift: the encoder's own reconstruction of every frame in a
/// moving sequence must match the libvpx oracle sample-for-sample. Inter frames
/// feed their reconstruction forward as the next reference, so any single-frame
/// drift (motion compensation offset, mvref/mode-context disagreement, coef ref
/// type, MV coding) compounds — this catches it at the first bad frame. Odd sizes
/// exercise mi padding and the partial bottom superblock row.
#[test]
fn m2b_inter_recon_matches_oracle_decode() {
    for &(w, h) in &[(176u32, 144u32), (640, 480), (636, 476)] {
        let frames: Vec<Vec<u8>> = (0..30).map(|t| moving_box(w, h, t)).collect();
        let mut enc = Vp9Encoder::new(config(w, h, 30, 500)).expect("encoder init");
        let mut dec = OracleDecoder::new().expect("oracle init failed");

        for (t, frame) in frames.iter().enumerate() {
            let ef = enc
                .encode(t as i64, frame)
                .expect("encode error")
                .expect("frame should be emitted");
            let recon = enc
                .last_reconstruction_i420()
                .expect("reconstruction available");
            let decoded = dec.decode(&ef.data).expect("oracle decode failed");
            assert_eq!(decoded.len(), 1, "frame {t} {w}x{h}: expected one frame");
            let d = &decoded[0];
            assert_eq!((d.width, d.height), (w, h), "frame {t} {w}x{h}: dims");
            assert_eq!(
                d.i420.len(),
                recon.len(),
                "frame {t} {w}x{h}: buffer length mismatch"
            );
            if d.i420 != recon {
                let first = d
                    .i420
                    .iter()
                    .zip(recon.iter())
                    .position(|(a, b)| a != b)
                    .unwrap();
                let diffs = d
                    .i420
                    .iter()
                    .zip(recon.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                panic!(
                    "frame {t} ({}) {w}x{h}: recon drift — {diffs} samples differ, first at \
                     byte {first} (oracle {} vs recon {})",
                    if ef.is_keyframe { "key" } else { "inter" },
                    d.i420[first],
                    recon[first]
                );
            }
        }
    }
}

/// Bit-exact reconstruction under a *changing per-frame quantizer*. Sweeping the
/// target bitrate frame-to-frame makes the rate controller pick a wide range of
/// qindices; each must still round-trip through the oracle sample-for-sample.
/// This catches any place where a per-frame qindex fails to propagate
/// consistently to the uncompressed header's `base_qindex`, the dequant tables,
/// and the coefficient tokenization. Uses the quantizer-gradable [`busy`] source
/// (see [`m3_bitrate_within_tolerance_over_150_frames`]) so the sweep actually
/// moves the qindex; `moving_box` above covers motion-vector drift.
#[test]
fn m2b_recon_matches_oracle_under_varying_qindex() {
    for &(w, h) in &[(176u32, 144u32), (640, 480)] {
        // Geometric target sweep from very low (→ high qindex) to very high
        // (→ low qindex) across the sequence.
        let bitrate_for = |t: usize| -> u32 { (60.0 * 1.18f64.powi(t as i32)) as u32 };

        let mut enc = Vp9Encoder::new(config(w, h, 30, bitrate_for(0))).expect("encoder init");
        let mut dec = OracleDecoder::new().expect("oracle init failed");
        let mut qindices: Vec<u8> = Vec::new();

        for t in 0..30usize {
            enc.update_bitrate_kbps(bitrate_for(t))
                .expect("bitrate update");
            let frame = busy(w, h, t as u32);
            let ef = enc
                .encode(t as i64, &frame)
                .expect("encode error")
                .expect("frame should be emitted");
            qindices.push(enc.last_base_qindex());
            let recon = enc
                .last_reconstruction_i420()
                .expect("reconstruction available");
            let decoded = dec.decode(&ef.data).expect("oracle decode failed");
            assert_eq!(decoded.len(), 1, "frame {t} {w}x{h}: expected one frame");
            let d = &decoded[0];
            assert_eq!((d.width, d.height), (w, h), "frame {t} {w}x{h}: dims");
            assert_eq!(d.i420.len(), recon.len(), "frame {t} {w}x{h}: length");
            if d.i420 != recon {
                let first = d
                    .i420
                    .iter()
                    .zip(recon.iter())
                    .position(|(a, b)| a != b)
                    .unwrap();
                panic!(
                    "frame {t} (q={}) {w}x{h}: recon drift at byte {first} (oracle {} vs recon {})",
                    qindices[t], d.i420[first], recon[first]
                );
            }
        }

        // Prove the sweep really exercised a changing quantizer.
        let mut distinct = qindices.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() >= 3,
            "{w}x{h}: expected >=3 distinct qindices across the sweep, got {distinct:?}"
        );
    }
}

#[test]
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

/// M3 uses 320x240 (not 640x480 like M1/M2) deliberately. The stage-6 encoder
/// signals a fixed full-split-to-8x8 partition with default (error-resilient)
/// probabilities, so every 8x8 block pays a small, quantizer-independent
/// mode/partition/skip cost. That per-block floor is ~0.9 bytes, i.e. ~1050 kbps
/// for a *fully static* 640x480 inter frame — above the 250..=750 band — so a
/// 500 kbps target is physically unreachable at 640x480 regardless of source or
/// quantizer. It scales with block count, so at 320x240 the floor is ~270 kbps,
/// well below the target, and the rate controller genuinely governs the bitrate.
/// (16x16/64x64 partitions and skip-run coding, which would lower the floor, are
/// future work.) The M3 source is [`busy`], whose mid-amplitude broadband
/// texture is quantizer-gradable — unlike `moving_box`, whose sharp edges cost a
/// fixed number of bits at any quantizer and leave nothing for the controller to
/// steer.
#[test]
fn m3_bitrate_within_tolerance_over_150_frames() {
    let (w, h, fps, kbps) = (320u32, 240u32, 30u32, 500u32);
    let n = 150usize;
    let frames: Vec<Vec<u8>> = (0..n as u32).map(|t| busy(w, h, t)).collect();

    let mut enc = Vp9Encoder::new(config(w, h, fps, kbps)).expect("encoder init");
    let encoded = encode_sequence(&mut enc, &frames);

    // Every frame must still decode on the oracle at these varying quantizers.
    let decoded = decode_all(&encoded);
    assert_eq!(decoded.len(), frames.len(), "all frames must decode");

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

/// Uses 320x240 with the quantizer-gradable [`busy`] source for the same reason
/// as [`m3_bitrate_within_tolerance_over_150_frames`]: only there does the rate
/// controller actually govern the bitrate, so quadrupling the target visibly
/// moves the coded size.
#[test]
fn m4_update_bitrate_kbps_takes_effect() {
    let (w, h, fps) = (320u32, 240u32, 30u32);
    let mut enc = Vp9Encoder::new(config(w, h, fps, 300)).expect("encoder init");

    // Low-bitrate phase: 60 frames @300 kbps, summing bytes but excluding the
    // opening keyframe (which is bitrate-independent and would skew the ratio).
    let mut bytes_lo = 0usize;
    for t in 0..60u32 {
        let frame = busy(w, h, t);
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
        let frame = busy(w, h, t);
        let _ = enc.encode(t as i64, &frame).expect("encode failed");
    }

    // High-bitrate phase: 60 frames @1200 kbps.
    let mut bytes_hi = 0usize;
    for t in 65..125u32 {
        let frame = busy(w, h, t);
        if let Some(ef) = enc.encode(t as i64, &frame).expect("encode failed") {
            bytes_hi += ef.data.len();
        }
    }

    assert!(
        bytes_hi as f64 >= 2.0 * bytes_lo as f64,
        "high-bitrate bytes {bytes_hi} should be >= 2x low-bitrate bytes {bytes_lo}"
    );
}

#[test]
#[ignore = "informational: prints M2 stream size, PSNR, timing"]
fn m2_report_metrics() {
    let (w, h, fps) = (640u32, 480u32, 30u32);
    let frames: Vec<Vec<u8>> = (0..90).map(|t| moving_box(w, h, t)).collect();
    let mut enc = Vp9Encoder::new(config(w, h, fps, 500)).expect("encoder init");
    let t0 = std::time::Instant::now();
    let encoded = encode_sequence(&mut enc, &frames);
    let elapsed = t0.elapsed();

    let total: usize = encoded.iter().map(|f| f.data.len()).sum();
    let key_bytes = encoded[0].data.len();
    let decoded = decode_all(&encoded);
    let ys: Vec<f64> = decoded
        .iter()
        .zip(frames.iter())
        .map(|(d, src)| psnr_i420(src, &d.i420, w, h).y)
        .collect();
    let avg = ys.iter().sum::<f64>() / ys.len() as f64;
    let min = ys.iter().copied().fold(f64::INFINITY, f64::min);
    eprintln!(
        "M2 90 frames {w}x{h}: total {total} bytes (keyframe {key_bytes}), \
         avg PSNR-Y {avg:.2} dB, min {min:.2} dB, encode {:.1} ms total = {:.2} ms/frame (debug build)",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e3 / 90.0
    );
}
