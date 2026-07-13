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
//! The implementation is being built up behind an oracle-based TDD harness
//! (encode with this encoder, decode with libvpx, assert PSNR/correctness). The
//! milestones live in `tests/oracle_vp9.rs`. This module is currently a stub:
//! [`Vp9Encoder::encode`] returns an error until the pipeline lands.
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
use anyhow::Result;

pub(crate) mod common;
#[cfg(any(test, feature = "test-utils"))]
pub(crate) mod debug;
pub(crate) mod enc;

/// Fixed base qindex used until rate control lands (stage 6). Roughly the middle
/// of the app's default q window.
const M1_BASE_QINDEX: u8 = 150;

/// The pure-Rust VP9 encoder.
///
/// Construct via [`Encodable::new`]. The first frame (and every
/// `keyframe_interval`-th frame) is an intra keyframe at a fixed quantizer; the
/// rest are single-reference inter frames with integer-pel motion compensation.
/// Rate control lands in a later milestone.
pub struct Vp9Encoder {
    config: EncoderConfig,
    frame_count: u64,
    /// Reconstruction of the most recently encoded frame with borders extended:
    /// the LAST reference for the next inter frame. Also exposed (via the
    /// cropped export) for the bit-exact oracle drift test.
    reference: Option<FrameBuffer>,
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

    /// Whether the next call to [`Encodable::encode`] will emit a keyframe (the
    /// first frame, then every `keyframe_interval`-th frame).
    fn is_keyframe(&self) -> bool {
        let interval = self.config.keyframe_interval.max(1) as u64;
        self.reference.is_none() || self.frame_count.is_multiple_of(interval)
    }
}

impl Encodable for Vp9Encoder {
    fn new(config: EncoderConfig) -> Result<Self> {
        Ok(Self {
            config,
            frame_count: 0,
            reference: None,
        })
    }

    fn update_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        self.config.bitrate_kbps = kbps;
        Ok(())
    }

    fn encode(&mut self, pts: i64, i420: &[u8]) -> Result<Option<EncodedFrame>> {
        let (w, h) = (self.config.width, self.config.height);
        let mut src = FrameBuffer::new(w, h);
        src.import_i420(i420, w, h)
            .map_err(|e| anyhow::anyhow!("i420 import failed: {e}"))?;

        let is_keyframe = self.is_keyframe();
        let (data, mut recon) = match (is_keyframe, self.reference.as_ref()) {
            (false, Some(reference)) => encode_inter_frame(&src, reference, M1_BASE_QINDEX),
            _ => encode_keyframe(&src, M1_BASE_QINDEX),
        };

        // The reconstruction becomes the LAST reference; extend its borders so
        // motion compensation of the next frame can read past the frame edge.
        recon.extend_borders();
        self.reference = Some(recon);
        self.frame_count += 1;

        Ok(Some(EncodedFrame {
            data,
            is_keyframe,
            pts,
        }))
    }
}
