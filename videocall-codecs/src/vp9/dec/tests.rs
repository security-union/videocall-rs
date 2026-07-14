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

use super::{decode_keyframe, DecodeError};
use crate::testing::i420::{busy, gradient, moving_box};
use crate::vp9::common::block::TxMode;
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::enc::bitstream::{pack_frame_header, UncompressedHeader};
use crate::vp9::enc::encoder::encode_keyframe;

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

/// Malformed input is rejected without panicking.
#[test]
fn rejects_garbage() {
    assert!(decode_keyframe(&[]).is_err());
    assert!(decode_keyframe(&[0x00, 0x00, 0x00]).is_err());
    assert!(decode_keyframe(&[0xff; 32]).is_err());
}

/// An untrusted header that declares an enormous width or height must be rejected
/// with `TooLarge` at the header boundary — no panic, no gigabyte allocation. The
/// header is well-formed (built by the encoder's writer) apart from its size, so
/// only the dimension bound stops it before `FrameBuffer::new` is ever reached.
#[test]
fn rejects_oversized_dimensions_without_allocating() {
    for (w, h) in [(65535u32, 64u32), (64, 65535), (65535, 65535)] {
        let header = UncompressedHeader::keyframe(w, h, 96);
        let (bytes, _) = pack_frame_header(&header, TxMode::Allow8X8);
        assert!(
            matches!(decode_keyframe(&bytes), Err(DecodeError::TooLarge)),
            "dimensions {w}x{h} must be rejected as TooLarge"
        );
    }
}
