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

//! Milestone 0 oracle: encode a keyframe with the pure-Rust encoder, decode it
//! with the pure-Rust decoder, and assert the reconstruction is byte-identical to
//! the encoder's own reconstruction buffer. Where libvpx is available, also
//! cross-check that the pure-Rust decode equals the libvpx decode — so all three
//! (encoder-recon, our-decode, libvpx-decode) agree bit-for-bit.

use super::{decode_keyframe, DecodeError, Vp9Decoder};
use crate::testing::i420::{busy, gradient, moving_box};
use crate::vp9::common::block::{mi_cols, tile_cols_log2_range, TxMode};
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::enc::bitstream::{pack_frame_header, UncompressedHeader};
use crate::vp9::enc::encoder::{encode_inter_frame, encode_keyframe};
use crate::vp9::enc::speed::SpeedFeatures;

/// Import a tight-packed I420 buffer into an encoder frame buffer.
fn src_fb(i420: &[u8], w: u32, h: u32) -> FrameBuffer {
    let mut fb = FrameBuffer::new(w, h);
    fb.import_i420(i420, w, h).expect("import i420");
    fb
}

/// Encode `i420` as a keyframe at `qindex`, decode it with the pure-Rust decoder,
/// and assert the decoded I420 equals the encoder's own reconstruction. When
/// libvpx is compiled in, also assert the libvpx decode matches.
fn assert_roundtrip(i420: &[u8], w: u32, h: u32, qindex: u8) {
    let src = src_fb(i420, w, h);
    let (bytes, recon) = encode_keyframe(&src, qindex);
    let encoder_recon = recon.export_i420();

    let decoded = decode_keyframe(&bytes).expect("pure-Rust decode failed");
    let our_i420 = decoded.export_i420();

    assert_eq!(
        our_i420.len(),
        encoder_recon.len(),
        "decoded I420 size mismatch at {w}x{h} q{qindex}"
    );
    assert_eq!(
        our_i420, encoder_recon,
        "pure-Rust decode != encoder reconstruction at {w}x{h} q{qindex}"
    );
    assert_eq!(
        (decoded.crop_width, decoded.crop_height),
        (w, h),
        "decoded crop dimensions wrong"
    );

    // Cross-check against the libvpx oracle: our decode must equal libvpx's.
    #[cfg(all(feature = "libvpx", not(target_arch = "wasm32")))]
    {
        use crate::testing::oracle::OracleDecoder;
        let mut dec = OracleDecoder::new().expect("oracle init");
        let frames = dec.decode(&bytes).expect("oracle decode");
        assert_eq!(frames.len(), 1, "oracle emitted {} frames", frames.len());
        assert_eq!(
            our_i420, frames[0].i420,
            "pure-Rust decode != libvpx decode at {w}x{h} q{qindex}"
        );
    }
}

/// Single-tile keyframes (176x144 → 1 tile column): partition tree, DC intra,
/// coefficient tokens, and dequant/idct all exercised without tile prefixes.
#[test]
fn m0_keyframe_roundtrip_single_tile() {
    let (w, h) = (176u32, 144u32);
    for &q in &[40u8, 96, 160, 220] {
        assert_roundtrip(&gradient(w, h, 0), w, h, q);
        assert_roundtrip(&moving_box(w, h, 3), w, h, q);
        assert_roundtrip(&busy(w, h, 1), w, h, q);
    }
}

/// Multi-tile keyframes (640x480 → 2 tile columns): additionally exercises the
/// 4-byte big-endian tile-size prefixes, per-tile boolean readers, and tile-edge
/// context reset (left neighbor severed at the tile boundary).
#[test]
fn m0_keyframe_roundtrip_multi_tile() {
    let (w, h) = (640u32, 480u32);
    for &q in &[40u8, 96, 160, 220] {
        assert_roundtrip(&gradient(w, h, 0), w, h, q);
        assert_roundtrip(&moving_box(w, h, 5), w, h, q);
        assert_roundtrip(&busy(w, h, 2), w, h, q);
    }
}

/// A non-mi-aligned resolution (636x476 → active 640x480, partial bottom SB row
/// and right-edge mi padding) still round-trips byte-for-byte.
#[test]
fn m0_keyframe_roundtrip_odd_size() {
    let (w, h) = (636u32, 476u32);
    for &q in &[64u8, 128] {
        assert_roundtrip(&gradient(w, h, 0), w, h, q);
        assert_roundtrip(&moving_box(w, h, 2), w, h, q);
    }
}

/// Encode a keyframe followed by `n_inter` inter frames of a moving-box
/// sequence, decode the whole sequence with the stateful pure-Rust
/// [`Vp9Decoder`], and assert every decoded frame is byte-identical to the
/// encoder's own reconstruction for that frame. When libvpx is compiled in, also
/// assert the libvpx decode of the sequence matches — so encoder-recon,
/// our-decode, and libvpx-decode all agree bit-for-bit, frame by frame.
fn assert_sequence_roundtrip(w: u32, h: u32, qindex: u8, n_inter: u32) {
    let sf = SpeedFeatures::default();

    // Encode the sequence, capturing each frame's bitstream and the encoder's
    // reconstruction (the ground truth the decoder must reproduce). The reference
    // for each inter frame is the previous reconstruction with borders extended,
    // exactly as the production encoder feeds it.
    let mut encoded: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    let (kf_bytes, kf_recon) = encode_keyframe(&src_fb(&moving_box(w, h, 0), w, h), qindex);
    encoded.push((kf_bytes, kf_recon.export_i420()));
    let mut reference = kf_recon;
    reference.extend_borders();

    for t in 1..=n_inter {
        let src = src_fb(&moving_box(w, h, t), w, h);
        let (bytes, recon) = encode_inter_frame(&src, &reference, qindex, &sf);
        encoded.push((bytes, recon.export_i420()));
        reference = recon;
        reference.extend_borders();
    }

    // Pure-Rust decode: one stateful decoder across the whole sequence, so the
    // reference-frame management (keyframe refreshes all slots, inter refreshes
    // slot 0) is exercised end-to-end.
    let mut dec = Vp9Decoder::new();
    for (i, (bytes, enc_recon)) in encoded.iter().enumerate() {
        let frame = dec.decode_frame(bytes).expect("pure-Rust decode failed");
        assert_eq!(
            (frame.crop_width, frame.crop_height),
            (w, h),
            "frame {i} crop dimensions wrong at {w}x{h} q{qindex}"
        );
        assert_eq!(
            &frame.export_i420(),
            enc_recon,
            "pure-Rust decode != encoder reconstruction at frame {i} ({w}x{h} q{qindex})"
        );
    }

    // Cross-check against the libvpx oracle: a single decoder context fed the
    // whole sequence must reproduce the same reconstruction for every frame.
    #[cfg(all(feature = "libvpx", not(target_arch = "wasm32")))]
    {
        use crate::testing::oracle::OracleDecoder;
        let mut oracle = OracleDecoder::new().expect("oracle init");
        for (i, (bytes, enc_recon)) in encoded.iter().enumerate() {
            let frames = oracle.decode(bytes).expect("oracle decode");
            assert_eq!(
                frames.len(),
                1,
                "oracle emitted {} frames for sequence frame {i}",
                frames.len()
            );
            assert_eq!(
                &frames[0].i420, enc_recon,
                "libvpx decode != encoder reconstruction at frame {i} ({w}x{h} q{qindex})"
            );
        }
    }
}

/// Single-tile inter round-trip (176x144 → 1 tile column): keyframe + several
/// inter frames, each byte-identical to the encoder's reconstruction and to
/// libvpx. Exercises 8x8/16x16 inter leaves, ZEROMV/NEARESTMV/NEWMV, integer-pel
/// motion compensation, and multi-frame reference management.
#[test]
fn m1_inter_roundtrip_single_tile() {
    let (w, h) = (176u32, 144u32);
    for &q in &[40u8, 96, 160, 220] {
        assert_sequence_roundtrip(w, h, q, 4);
    }
}

/// Multi-tile inter round-trip (640x480 → 2 tile columns): additionally exercises
/// the per-tile boolean readers, tile-size prefixes, tile-edge context reset, and
/// the MV-reference column clamp at the tile boundary.
#[test]
fn m1_inter_roundtrip_multi_tile() {
    let (w, h) = (640u32, 480u32);
    for &q in &[64u8, 160] {
        assert_sequence_roundtrip(w, h, q, 3);
    }
}

/// A non-mi-aligned resolution (636x476 → active 640x480, partial bottom SB row
/// and right-edge mi padding) still inter round-trips byte-for-byte, exercising
/// the frame-edge motion-compensation clamp.
#[test]
fn m1_inter_roundtrip_odd_size() {
    let (w, h) = (636u32, 476u32);
    for &q in &[64u8, 128] {
        assert_sequence_roundtrip(w, h, q, 3);
    }
}

/// Many inter frames deep: confirms reference management stays correct as the
/// decoder chains reconstruction → reference → next reconstruction repeatedly,
/// with the box translating a different amount every frame.
#[test]
fn m1_inter_roundtrip_many_frames_deep() {
    assert_sequence_roundtrip(176, 144, 96, 12);
}

/// An inter frame decoded before any keyframe has no reference history and must be
/// rejected (not read uninitialized state or panic).
#[test]
fn rejects_inter_frame_without_reference() {
    let (w, h) = (176u32, 144u32);
    let (kf_bytes, kf_recon) = encode_keyframe(&src_fb(&moving_box(w, h, 0), w, h), 96);
    let mut reference = kf_recon;
    reference.extend_borders();
    let (inter_bytes, _) = encode_inter_frame(
        &src_fb(&moving_box(w, h, 1), w, h),
        &reference,
        96,
        &SpeedFeatures::default(),
    );
    // A fresh decoder (no keyframe seen) must refuse the inter frame.
    let mut dec = Vp9Decoder::new();
    assert!(matches!(
        dec.decode_frame(&inter_bytes),
        Err(DecodeError::Corrupt(_))
    ));
    // After a keyframe, the same inter frame decodes.
    let mut dec = Vp9Decoder::new();
    dec.decode_frame(&kf_bytes).expect("keyframe decodes");
    dec.decode_frame(&inter_bytes).expect("inter decodes");
}

/// Malformed input is rejected without panicking.
#[test]
fn rejects_garbage() {
    assert!(decode_keyframe(&[]).is_err());
    assert!(decode_keyframe(&[0x00, 0x00, 0x00]).is_err());
    assert!(decode_keyframe(&[0xff; 32]).is_err());
}

/// An untrusted header that declares dimensions beyond [`super::MAX_DIM`] (8192)
/// must be rejected with `TooLarge` at the header boundary — no panic, no gigabyte
/// allocation — before `FrameBuffer::new` is ever reached.
///
/// The header is built by the encoder's own writer so it is well-formed apart from
/// its size, isolating the decoder's dimension bound as the sole gate. `write_tile_info`
/// debug-asserts `log2_tile_cols >= min_log2_tile_cols` for the width, and wide
/// frames have a non-zero minimum, so each case sets `log2_tile_cols` to exactly
/// that width's minimum (≤ 2 for widths up to 16384). Dimensions stay ≤ 16384 so
/// the tile minimum is representable; the decoder still rejects them as > 8192.
#[test]
fn rejects_oversized_dimensions_without_allocating() {
    for (w, h) in [(16384u32, 64u32), (64, 16384), (16384, 16384)] {
        let mut header = UncompressedHeader::keyframe(w, h, 96);
        let (min_log2, _max_log2) = tile_cols_log2_range(mi_cols(w));
        header.log2_tile_cols = min_log2;
        let (bytes, _) = pack_frame_header(&header, TxMode::Allow8X8);
        assert!(
            matches!(decode_keyframe(&bytes), Err(DecodeError::TooLarge)),
            "dimensions {w}x{h} must be rejected as TooLarge"
        );
    }
}
