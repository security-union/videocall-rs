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

//! Pure-Rust VP9 encoder.
//!
//! This is a from-scratch, dependency-free VP9 encoder targeting a one-pass,
//! realtime, error-resilient subset of VP9 Profile 0 (8-bit I420). It is a
//! drop-in replacement for the C libvpx encoder used elsewhere in the project
//! and compiles on every target, including `wasm32`.
//!
//! The implementation was built up behind an oracle-based TDD harness (encode
//! with this encoder, decode with libvpx, assert PSNR/correctness); the
//! milestones live in `tests/oracle_vp9.rs`. It now encodes keyframes and
//! single-reference integer-pel inter frames, with one-pass VBR rate control and
//! `cpu_used` speed presets.
//!
//! ## Planned module layout
//!
//! - `common/` — decoder-mandated, bit-exact machinery ported faithfully from
//!   libvpx `vp9/common/` and `vpx_dsp/`: the boolean arithmetic coder, default
//!   probability tables, context derivations, scan orders, dequant tables,
//!   inverse transforms, intra predictors, motion-compensation filters, and
//!   header syntax. Errors here silently corrupt the bitstream.
//! - `enc/` — encoder-free choices that may be simplified aggressively: forward
//!   transforms, quantizer rounding, mode decision, motion search, rate control,
//!   and partitioning. Errors here only cost quality.
//! - `debug/` — a minimal stream parser for round-trip validation plus IVF glue.

use crate::encoder::{Encodable, EncodedFrame, EncoderConfig};
use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::enc::encoder::{encode_inter_frame, encode_keyframe};
use crate::vp9::enc::ratectrl::RateControl;
use crate::vp9::enc::speed::SpeedFeatures;
use anyhow::Result;

pub(crate) mod common;
#[cfg(any(test, feature = "test-utils"))]
pub(crate) mod debug;
pub mod dec;
pub(crate) mod enc;

/// Multiple of the per-frame keyframe target above which a keyframe is re-encoded
/// once at a higher quantizer to rein in a gross overshoot.
const KF_OVERSHOOT_MULT: i64 = 3;
/// Quantizer step applied to the one keyframe overshoot re-encode.
const KF_OVERSHOOT_QSTEP: i32 = 8;

/// The pure-Rust VP9 encoder.
///
/// Construct via [`Encodable::new`]. The first frame (and every
/// `keyframe_interval`-th frame, plus any frame after [`Vp9Encoder::force_keyframe`])
/// is an intra keyframe; the rest are single-reference inter frames with
/// integer-pel motion compensation. A one-pass VBR [`RateControl`] picks the
/// per-frame quantizer within the app's `[min_quantizer, max_quantizer]` window;
/// [`SpeedFeatures`] derived from `cpu_used` tune the motion search.
pub struct Vp9Encoder {
    config: EncoderConfig,
    frame_count: u64,
    /// Reconstruction of the most recently encoded frame with borders extended:
    /// the LAST reference for the next inter frame. Also exposed (via the
    /// cropped export) for the bit-exact oracle drift test.
    reference: Option<FrameBuffer>,
    /// One-pass VBR rate controller (target bitrate, qindex window, bit balance).
    rc: RateControl,
    /// `cpu_used`-derived speed knobs.
    sf: SpeedFeatures,
    /// Set by [`Vp9Encoder::force_keyframe`]; forces the next frame to be a
    /// keyframe, then clears.
    force_kf: bool,
    /// Base qindex chosen for the most recently encoded frame (0 before the
    /// first). Surfaced for rate-control drift tests via
    /// [`Vp9Encoder::last_base_qindex`]; unread in non-test builds.
    #[cfg_attr(not(any(test, feature = "test-utils")), allow(dead_code))]
    last_qindex: u8,
}

impl Vp9Encoder {
    /// The most recently encoded frame's reconstruction as a tight-packed I420
    /// buffer at the cropped dimensions, or `None` before the first frame.
    ///
    /// This is exactly what a conformant decoder must reproduce, so tests can
    /// assert bit-exactness against the libvpx oracle.
    pub fn last_reconstruction_i420(&self) -> Option<Vec<u8>> {
        self.reference.as_ref().map(|fb| fb.export_i420())
    }

    /// Base qindex the rate controller selected for the most recently encoded
    /// frame. Exposed for tests that assert the quantizer varies across a
    /// sequence (bit-exactness must hold under a changing per-frame qindex).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn last_base_qindex(&self) -> u8 {
        self.last_qindex
    }

    /// Request that the next encoded frame be a keyframe, regardless of the
    /// keyframe interval. The flag clears after that frame is emitted. Useful
    /// when the app needs a decodable refresh point (e.g. a new participant
    /// joins).
    pub fn force_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Whether the next call to [`Encodable::encode`] will emit a keyframe (the
    /// first frame, every `keyframe_interval`-th frame, or when forced).
    fn is_keyframe(&self) -> bool {
        let interval = self.config.keyframe_interval.max(1) as u64;
        self.force_kf || self.reference.is_none() || self.frame_count.is_multiple_of(interval)
    }

    /// Encode `src` at `qindex` as a keyframe, or as an inter frame against the
    /// current reference when one exists and `is_keyframe` is false.
    fn encode_at(
        &self,
        src: &FrameBuffer,
        is_keyframe: bool,
        qindex: u8,
    ) -> (Vec<u8>, FrameBuffer) {
        match (is_keyframe, self.reference.as_ref()) {
            (false, Some(reference)) => encode_inter_frame(src, reference, qindex, &self.sf),
            _ => encode_keyframe(src, qindex),
        }
    }
}

impl Encodable for Vp9Encoder {
    fn new(config: EncoderConfig) -> Result<Self> {
        let rc = RateControl::new(&config);
        let sf = SpeedFeatures::for_cpu_used(config.cpu_used);
        Ok(Self {
            config,
            frame_count: 0,
            reference: None,
            rc,
            sf,
            force_kf: false,
            last_qindex: 0,
        })
    }

    fn update_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        self.config.bitrate_kbps = kbps;
        self.rc.update_bitrate_kbps(kbps);
        Ok(())
    }

    fn encode(&mut self, pts: i64, i420: &[u8]) -> Result<Option<EncodedFrame>> {
        let (w, h) = (self.config.width, self.config.height);
        let mut src = FrameBuffer::new(w, h);
        src.import_i420(i420, w, h)
            .map_err(|e| anyhow::anyhow!("i420 import failed: {e}"))?;

        let is_keyframe = self.is_keyframe();
        let target_bits = self.rc.frame_target_bits(is_keyframe);
        let mut qindex = self.rc.select_qindex(is_keyframe, target_bits);
        let (mut data, mut recon) = self.encode_at(&src, is_keyframe, qindex);

        // Keyframe overshoot guard: a keyframe that blows far past its target is
        // re-encoded once at a higher quantizer (the two-phase pipeline makes a
        // one-shot re-encode cheap). Inter frames rely on the balance instead.
        if is_keyframe {
            let (_, qmax) = self.rc.qindex_window();
            let actual_bits = (data.len() as i64) * 8;
            if actual_bits > KF_OVERSHOOT_MULT * target_bits && (qindex as i32) < qmax {
                let q2 = ((qindex as i32) + KF_OVERSHOOT_QSTEP).min(qmax) as u8;
                let (d2, r2) = self.encode_at(&src, true, q2);
                data = d2;
                recon = r2;
                qindex = q2;
            }
        }

        let actual_bits = (data.len() as i64) * 8;
        self.rc
            .update_after_encode(is_keyframe, qindex, target_bits, actual_bits);
        self.last_qindex = qindex;

        // The reconstruction becomes the LAST reference; extend its borders so
        // motion compensation of the next frame can read past the frame edge.
        recon.extend_borders();
        self.reference = Some(recon);
        self.frame_count += 1;
        self.force_kf = false;

        Ok(Some(EncodedFrame {
            data,
            is_keyframe,
            pts,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(keyframe_interval: u32) -> EncoderConfig {
        EncoderConfig {
            width: 64,
            height: 64,
            framerate: 30,
            bitrate_kbps: 500,
            keyframe_interval,
            min_quantizer: 40,
            max_quantizer: 60,
            cpu_used: 7,
        }
    }

    /// A flat mid-gray I420 frame of the config's dimensions.
    fn gray(w: u32, h: u32) -> Vec<u8> {
        vec![128u8; (w * h + 2 * (w.div_ceil(2)) * (h.div_ceil(2))) as usize]
    }

    fn is_kf(enc: &mut Vp9Encoder, pts: i64) -> bool {
        enc.encode(pts, &gray(64, 64)).unwrap().unwrap().is_keyframe
    }

    #[test]
    fn first_frame_is_keyframe_rest_are_inter() {
        let mut enc = Vp9Encoder::new(cfg(150)).unwrap();
        assert!(is_kf(&mut enc, 0), "frame 0 must be a keyframe");
        for t in 1..10 {
            assert!(!is_kf(&mut enc, t), "frame {t} must be inter");
        }
    }

    #[test]
    fn keyframe_cadence_follows_interval() {
        let mut enc = Vp9Encoder::new(cfg(5)).unwrap();
        // Keyframes at 0, 5, 10; inter elsewhere.
        for t in 0..12i64 {
            let key = is_kf(&mut enc, t);
            let expect = t % 5 == 0;
            assert_eq!(key, expect, "frame {t}: keyframe={key}, expected {expect}");
        }
    }

    #[test]
    fn force_keyframe_forces_next_frame_only() {
        let mut enc = Vp9Encoder::new(cfg(150)).unwrap();
        assert!(is_kf(&mut enc, 0)); // frame 0 keyframe
        assert!(!is_kf(&mut enc, 1)); // frame 1 inter
        enc.force_keyframe();
        assert!(is_kf(&mut enc, 2), "forced frame must be a keyframe");
        assert!(!is_kf(&mut enc, 3), "flag must clear after one frame");
    }

    #[test]
    fn qindex_stays_within_configured_window() {
        // q 40..60 → qindex window [160, 240].
        let mut enc = Vp9Encoder::new(cfg(150)).unwrap();
        for t in 0..8i64 {
            enc.encode(t, &gray(64, 64)).unwrap();
            let q = enc.last_base_qindex();
            assert!((160..=240).contains(&q), "qindex {q} outside window");
        }
    }
}
