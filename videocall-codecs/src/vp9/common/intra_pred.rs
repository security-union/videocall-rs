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

//! Intra prediction — DC_PRED, bit-exact with the VP9 decoder.
//!
//! [`predict_dc`] ports the four DC predictor variants from libvpx
//! `vpx_dsp/intrapred.c` (`dc_128_predictor`, `dc_left_predictor`,
//! `dc_top_predictor`, `dc_predictor`), selected by neighbor availability
//! exactly as `dc_pred[left_available][up_available][tx_size]` does.
//!
//! [`build_intra_dc`] ports the DC-relevant parts of `build_intra_predictors`
//! (`vp9/common/vp9_reconintra.c`): reading the above row and left column out
//! of the reconstruction buffer, the `127`/`129` fills for unavailable edges,
//! and frame-edge replication for blocks whose transform runs past the (mi-
//! aligned) frame dimensions. Only `NEED_ABOVE | NEED_LEFT` (DC) is handled;
//! `NEED_ABOVERIGHT` and the directional modes arrive with later milestones.

use crate::vp9::common::block::TxSize;

/// Fill value for an unavailable above row (libvpx `127`).
const ABOVE_FILL: u8 = 127;
/// Fill value for an unavailable left column (libvpx `129`).
const LEFT_FILL: u8 = 129;

/// Transform side length in pixels: `bs = 4 << tx_size`.
#[inline]
fn block_size(tx_size: TxSize) -> usize {
    4 << (tx_size as usize)
}

/// Write a `bs x bs` DC prediction into `dst` (block top-left at `dst_off`,
/// `dst_stride` samples per row).
///
/// `above`/`left` supply the (already edge-resolved) neighbor samples; only the
/// first `bs` entries of each are read, and only when the corresponding
/// availability flag is set. Port of the `dc_pred[left][above][tx]` dispatch.
#[allow(clippy::too_many_arguments)]
pub fn predict_dc(
    tx_size: TxSize,
    have_above: bool,
    have_left: bool,
    above: &[u8],
    left: &[u8],
    dst: &mut [u8],
    dst_off: usize,
    dst_stride: usize,
) {
    let bs = block_size(tx_size);
    let expected_dc: u8 = match (have_left, have_above) {
        (false, false) => 128,
        (false, true) => {
            let sum: u32 = above[..bs].iter().map(|&p| p as u32).sum();
            ((sum + (bs as u32 >> 1)) / bs as u32) as u8
        }
        (true, false) => {
            let sum: u32 = left[..bs].iter().map(|&p| p as u32).sum();
            ((sum + (bs as u32 >> 1)) / bs as u32) as u8
        }
        (true, true) => {
            let count = 2 * bs as u32;
            let sum: u32 = above[..bs]
                .iter()
                .chain(left[..bs].iter())
                .map(|&p| p as u32)
                .sum();
            ((sum + (count >> 1)) / count) as u8
        }
    };
    for r in 0..bs {
        let row = dst_off + r * dst_stride;
        dst[row..row + bs].fill(expected_dc);
    }
}

/// Predict a DC block in place inside a bordered reconstruction buffer.
///
/// `buf` holds the reconstruction plane; the block's top-left sample is at
/// `off` with `stride` samples per row. `x0`/`y0` are the block's position in
/// plane pixels and `frame_w`/`frame_h` the (mi-aligned) plane dimensions,
/// used for edge replication. Neighbor samples are read from `buf` (the border
/// must be wide enough that `off >= stride + 1`) before the prediction is
/// written, so a single mutable buffer is safe.
#[allow(clippy::too_many_arguments)]
pub fn build_intra_dc(
    buf: &mut [u8],
    off: usize,
    stride: usize,
    tx_size: TxSize,
    up_available: bool,
    left_available: bool,
    x0: i32,
    y0: i32,
    frame_w: i32,
    frame_h: i32,
) {
    let bs = block_size(tx_size);
    let mut above = [ABOVE_FILL; 32];
    let mut left = [LEFT_FILL; 32];

    // Left column (NEED_LEFT).
    if left_available {
        // Number of real rows before running past the frame bottom.
        let copy = (frame_h - y0).clamp(0, bs as i32) as usize;
        for (i, l) in left.iter_mut().enumerate().take(copy) {
            *l = buf[off + i * stride - 1];
        }
        // Replicate the last real sample downward.
        if copy > 0 {
            let last = left[copy - 1];
            for l in left.iter_mut().take(bs).skip(copy) {
                *l = last;
            }
        }
    }

    // Above row (NEED_ABOVE).
    if up_available {
        let above_off = off - stride;
        let copy = (frame_w - x0).clamp(0, bs as i32) as usize;
        for (i, a) in above.iter_mut().enumerate().take(copy) {
            *a = buf[above_off + i];
        }
        if copy > 0 {
            let last = above[copy - 1];
            for a in above.iter_mut().take(bs).skip(copy) {
                *a = last;
            }
        }
    }

    predict_dc(
        tx_size,
        up_available,
        left_available,
        &above,
        &left,
        buf,
        off,
        stride,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_block(dst: &[u8], off: usize, stride: usize, bs: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for r in 0..bs {
            v.extend_from_slice(&dst[off + r * stride..off + r * stride + bs]);
        }
        v
    }

    #[test]
    fn dc_128_when_no_neighbors() {
        let mut dst = [0u8; 16];
        predict_dc(TxSize::Tx4X4, false, false, &[], &[], &mut dst, 0, 4);
        assert!(dst.iter().all(|&p| p == 128));
    }

    #[test]
    fn dc_top_only_above() {
        // above averages to (10+20+30+40+2)/4 = 25 (rounded).
        let above = [10u8, 20, 30, 40];
        let mut dst = [0u8; 16];
        predict_dc(TxSize::Tx4X4, true, false, &above, &[], &mut dst, 0, 4);
        assert!(dst.iter().all(|&p| p == 25));
    }

    #[test]
    fn dc_left_only_left() {
        let left = [100u8, 100, 100, 100];
        let mut dst = [0u8; 16];
        predict_dc(TxSize::Tx4X4, false, true, &[], &left, &mut dst, 0, 4);
        assert!(dst.iter().all(|&p| p == 100));
    }

    #[test]
    fn dc_both_averages_above_and_left() {
        // sum = (4*50 + 4*70) = 480; count=8; (480+4)/8 = 60.
        let above = [50u8; 4];
        let left = [70u8; 4];
        let mut dst = [0u8; 16];
        predict_dc(TxSize::Tx4X4, true, true, &above, &left, &mut dst, 0, 4);
        assert!(dst.iter().all(|&p| p == 60));
    }

    #[test]
    fn dc_8x8_rounding() {
        // 8 above=1, 8 left=2 -> sum 24, count 16, (24+8)/16 = 2.
        let above = [1u8; 8];
        let left = [2u8; 8];
        let mut dst = [0u8; 64];
        predict_dc(TxSize::Tx8X8, true, true, &above, &left, &mut dst, 0, 8);
        assert!(dst.iter().all(|&p| p == 2));
    }

    /// build_intra_dc pulls the correct above/left samples from a bordered
    /// buffer and, at the top-left corner (no neighbors), fills 128.
    #[test]
    fn build_reads_neighbors_from_buffer() {
        let stride = 16;
        let border = 4;
        let mut buf = vec![0u8; stride * 16];
        // Place a 4x4 block at plane (0,0) -> buffer offset (border,border).
        let off = border * stride + border;
        // Fill the row above the block and the column to its left.
        for i in 0..4 {
            buf[off - stride + i] = 80; // above row
            buf[off + i * stride - 1] = 40; // left column
        }
        // Top-left block: no neighbors available -> 128.
        build_intra_dc(
            &mut buf,
            off,
            stride,
            TxSize::Tx4X4,
            false,
            false,
            0,
            0,
            64,
            64,
        );
        assert_eq!(read_block(&buf, off, stride, 4), vec![128u8; 16]);

        // Now mark both available: DC = (4*80 + 4*40 + 4)/8 = 60.
        build_intra_dc(
            &mut buf,
            off,
            stride,
            TxSize::Tx4X4,
            true,
            true,
            8,
            8,
            64,
            64,
        );
        assert_eq!(read_block(&buf, off, stride, 4), vec![60u8; 16]);
    }

    /// When the block runs past the frame's right edge, the above row replicates
    /// its last in-frame sample.
    #[test]
    fn build_replicates_past_right_edge() {
        let stride = 32;
        let border = 8;
        let mut buf = vec![0u8; stride * 32];
        let off = border * stride + border;
        // Above row: first two columns in-frame (10, 20), rest garbage.
        buf[off - stride] = 10;
        buf[off - stride + 1] = 20;
        buf[off - stride + 2] = 200;
        buf[off - stride + 3] = 200;
        // frame_w = x0 + 2, so only 2 samples are in-frame; the other 2 replicate 20.
        // Above-only DC: (10 + 20 + 20 + 20 + 2) / 4 = 18.
        build_intra_dc(
            &mut buf,
            off,
            stride,
            TxSize::Tx4X4,
            true,
            false,
            10,
            8,
            12,
            64,
        );
        assert_eq!(read_block(&buf, off, stride, 4), vec![18u8; 16]);
    }
}
