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
use crate::vp9::enc::encoder::encode_keyframe;
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
/// Construct via [`Encodable::new`]. Every frame is currently encoded as an
/// intra keyframe at a fixed quantizer (inter prediction and rate control land
/// in later milestones).
pub struct Vp9Encoder {
    config: EncoderConfig,
    frame_count: u64,
    /// Reconstruction of the most recently encoded frame (the future inter-frame
    /// reference; also exposed for the bit-exact oracle drift test).
    last_recon: Option<FrameBuffer>,
}

impl Vp9Encoder {
    /// The most recently encoded frame's reconstruction as a tight-packed I420
    /// buffer at the cropped dimensions, or `None` before the first frame.
    ///
    /// This is exactly what a conformant decoder must reproduce, so tests can
    /// assert bit-exactness against the libvpx oracle.
    pub fn last_reconstruction_i420(&self) -> Option<Vec<u8>> {
        self.last_recon.as_ref().map(|fb| fb.export_i420())
    }
}

impl Encodable for Vp9Encoder {
    fn new(config: EncoderConfig) -> Result<Self> {
        Ok(Self {
            config,
            frame_count: 0,
            last_recon: None,
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

        let (data, recon) = encode_keyframe(&src, M1_BASE_QINDEX);
        self.frame_count += 1;
        self.last_recon = Some(recon);

        Ok(Some(EncodedFrame {
            data,
            is_keyframe: true,
            pts,
        }))
    }
}
