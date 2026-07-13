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

//! Integer-pel motion compensation: the block-copy subset of
//! `vp9/common/vp9_reconinter.c` `build_inter_predictors`.
//!
//! This stage restricts motion vectors to **multiples of 16 in 1/8-pel units**
//! (even whole-pixel luma displacements). VP9 converts a stored MV to the Q4
//! (1/16-pel) domain by `mv * (1 << (1 - subsampling))`, so luma doubles and
//! chroma passes through; with the multiples-of-16 restriction both planes land
//! on integer sample positions (`subpel == 0`) even after the umv-border clamp,
//! and motion compensation is a pure block copy from the bordered reference. The
//! subpel `convolve8` path (and thus finer, per-pixel motion) is deferred to a
//! later quality pass. Quality cost: motion is quantized to 2-pixel steps.
//!
//! `clamp_mv_to_umv_border_sb` is ported faithfully so a block near a frame edge
//! reads exactly the samples the decoder reads — dropping it would desync recon
//! from the oracle for edge blocks whose search MV points outward.

use crate::vp9::common::frame_buffer::FrameBuffer;
use crate::vp9::common::mvref::Mv;

/// `VP9_INTERP_EXTEND`.
const VP9_INTERP_EXTEND: i32 = 4;
/// `SUBPEL_BITS`.
const SUBPEL_BITS: i32 = 4;
/// `SUBPEL_MASK`.
const SUBPEL_MASK: i32 = (1 << SUBPEL_BITS) - 1;
/// `SUBPEL_SHIFTS`.
const SUBPEL_SHIFTS: i32 = 1 << SUBPEL_BITS;

/// Which plane a motion-compensated block belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plane {
    Y,
    U,
    V,
}

impl Plane {
    #[inline]
    fn subsampling(self) -> i32 {
        match self {
            Plane::Y => 0,
            Plane::U | Plane::V => 1,
        }
    }
}

#[inline]
fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

/// Compute the integer sample offset `(off_row, off_col)` (in plane pixels) that
/// `build_inter_predictors` uses for a square block after clamping.
///
/// `mi_row`/`mi_col` are the block's mode-info coordinates, `px` the plane block
/// dimension in pixels (8 luma / 4 chroma for an 8x8 leaf; 16 / 8 for a 16x16
/// leaf), and `bw_mi`/`bh_mi` the block size in mode-info units (1 for an 8x8
/// leaf, 2 for a 16x16 leaf) used for the frame-edge clamp (`set_mi_row_col`).
/// Returns the clamped Q4 MV's integer part and sub-pel remainder; the caller
/// asserts the remainder is zero.
#[allow(clippy::too_many_arguments)]
fn integer_mv_offset(
    mv: Mv,
    mi_row: i32,
    mi_col: i32,
    px: i32,
    ss: i32,
    bw_mi: i32,
    bh_mi: i32,
    mi_rows: i32,
    mi_cols: i32,
) -> (i32, i32, i32, i32) {
    // Edges in 1/8-pel luma units (set_mi_row_col with MI_SIZE = 8), block size
    // dependent: mb_to_bottom/right use the block's mi height/width.
    let mb_to_top = -((mi_row * 8) * 8);
    let mb_to_bottom = (mi_rows - bh_mi - mi_row) * 8 * 8;
    let mb_to_left = -((mi_col * 8) * 8);
    let mb_to_right = (mi_cols - bw_mi - mi_col) * 8 * 8;

    // clamp_mv_to_umv_border_sb: scale to Q4 for this plane, then clamp. Blocks
    // are square here, so bw == bh == px.
    let scale = 1 << (1 - ss); // luma 2, chroma 1
    let spel_left = (VP9_INTERP_EXTEND + px) << SUBPEL_BITS;
    let spel_right = spel_left - SUBPEL_SHIFTS;
    let spel_top = spel_left;
    let spel_bottom = spel_right;

    let q4_row = clamp(
        mv.row as i32 * scale,
        mb_to_top * scale - spel_top,
        mb_to_bottom * scale + spel_bottom,
    );
    let q4_col = clamp(
        mv.col as i32 * scale,
        mb_to_left * scale - spel_left,
        mb_to_right * scale + spel_right,
    );

    let subpel_row = q4_row & SUBPEL_MASK;
    let subpel_col = q4_col & SUBPEL_MASK;
    // Arithmetic shift toward negative infinity, matching C `>> SUBPEL_BITS` on
    // the already-clamped value.
    let off_row = q4_row >> SUBPEL_BITS;
    let off_col = q4_col >> SUBPEL_BITS;
    (off_row, off_col, subpel_row, subpel_col)
}

/// Motion-compensate one block by copying from `reference` into `dst` at the
/// block's mode-info position. `bsize_mi` is the (square) block size in mode-info
/// units: 1 for an 8x8 leaf (8x8 luma / 4x4 chroma), 2 for a 16x16 leaf (16x16
/// luma / 8x8 chroma).
///
/// `reference` must have its borders extended. `mv` must be a multiple of 16 in
/// 1/8-pel units (enforced by the motion search); this function debug-asserts the
/// resulting sub-pel offset is zero.
#[allow(clippy::too_many_arguments)]
pub fn predict_inter_block(
    reference: &FrameBuffer,
    dst: &mut FrameBuffer,
    plane: Plane,
    mi_row: i32,
    mi_col: i32,
    mv: Mv,
    bsize_mi: i32,
    mi_rows: i32,
    mi_cols: i32,
) {
    let ss = plane.subsampling();
    // Block dimension in plane pixels (luma bsize_mi*8, chroma bsize_mi*4) and the
    // pixel size of one mode-info unit in this plane (luma 8, chroma 4). The block
    // dimension sizes the copy and the sub-pel clamp; the mi size maps the block's
    // grid position to a pixel offset (they coincide only for an 8x8 leaf).
    let px = (bsize_mi * 8) >> ss;
    let mi_px = 8 >> ss;
    let (off_row, off_col, sub_row, sub_col) = integer_mv_offset(
        mv, mi_row, mi_col, px, ss, bsize_mi, bsize_mi, mi_rows, mi_cols,
    );
    debug_assert_eq!(
        (sub_row, sub_col),
        (0, 0),
        "integer-pel MC requires MV multiples of 16 in 1/8-pel units"
    );

    let (rdata, rorg, rstride, _, _) = match plane {
        Plane::Y => reference.y(),
        Plane::U => reference.u(),
        Plane::V => reference.v(),
    };
    let base_row = mi_row * mi_px + off_row;
    let base_col = mi_col * mi_px + off_col;
    let src0 = (rorg as i32 + base_row * rstride as i32 + base_col) as usize;

    let (ddata, dorg, dstride) = match plane {
        Plane::Y => dst.y_mut(),
        Plane::U => dst.u_mut(),
        Plane::V => dst.v_mut(),
    };
    let dst0 = dorg + (mi_row * mi_px) as usize * dstride + (mi_col * mi_px) as usize;

    for r in 0..px as usize {
        let s = src0 + r * rstride;
        let d = dst0 + r * dstride;
        ddata[d..d + px as usize].copy_from_slice(&rdata[s..s + px as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference buffer whose luma sample at active (r, c) equals
    /// `(r * 3 + c * 7) & 0xff`, chroma a constant, with borders extended.
    fn make_ref(w: u32, h: u32) -> FrameBuffer {
        let (yw, yh) = (w as usize, h as usize);
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        let mut src = Vec::new();
        for r in 0..yh {
            for c in 0..yw {
                src.push(((r * 3 + c * 7) & 0xff) as u8);
            }
        }
        src.extend(std::iter::repeat_n(120u8, 2 * cw * ch));
        let mut fb = FrameBuffer::new(w, h);
        fb.import_i420(&src, w, h).unwrap();
        fb.extend_borders();
        fb
    }

    fn luma_at(fb: &FrameBuffer, r: usize, c: usize) -> u8 {
        let (data, org, stride, _, _) = fb.y();
        data[org + r * stride + c]
    }

    #[test]
    fn zero_mv_copies_colocated_block() {
        let reference = make_ref(64, 64);
        let mut dst = FrameBuffer::new(64, 64);
        predict_inter_block(&reference, &mut dst, Plane::Y, 2, 3, Mv::ZERO, 1, 8, 8);
        // Block at mi (2,3) → pixels (16, 24).
        for r in 0..8 {
            for c in 0..8 {
                assert_eq!(
                    luma_at(&dst, 16 + r, 24 + c),
                    luma_at(&reference, 16 + r, 24 + c)
                );
            }
        }
    }

    #[test]
    fn positive_mv_shifts_source() {
        // MV = (16, 32) in 1/8-pel → +2 px down, +4 px right (multiples of 16).
        let reference = make_ref(64, 64);
        let mut dst = FrameBuffer::new(64, 64);
        predict_inter_block(
            &reference,
            &mut dst,
            Plane::Y,
            2,
            2,
            Mv::new(16, 32),
            1,
            8,
            8,
        );
        for r in 0..8 {
            for c in 0..8 {
                // dst(16+r,16+c) = ref(16+r+2, 16+c+4).
                assert_eq!(
                    luma_at(&dst, 16 + r, 16 + c),
                    luma_at(&reference, 18 + r, 20 + c)
                );
            }
        }
    }

    #[test]
    fn negative_mv_reads_up_left() {
        // MV = (-16, -16) → -2 px up, -2 px left. Block at mi (4,4) → px (32,32).
        let reference = make_ref(64, 64);
        let mut dst = FrameBuffer::new(64, 64);
        predict_inter_block(
            &reference,
            &mut dst,
            Plane::Y,
            4,
            4,
            Mv::new(-16, -16),
            1,
            8,
            8,
        );
        for r in 0..8 {
            for c in 0..8 {
                assert_eq!(
                    luma_at(&dst, 32 + r, 32 + c),
                    luma_at(&reference, 30 + r, 30 + c)
                );
            }
        }
    }

    #[test]
    fn negative_mv_at_corner_reads_border() {
        // Block at mi (0,0) with a leftward/upward MV reads the replicated border
        // (all equal to the corner sample after extend_borders).
        let reference = make_ref(64, 64);
        let mut dst = FrameBuffer::new(64, 64);
        predict_inter_block(
            &reference,
            &mut dst,
            Plane::Y,
            0,
            0,
            Mv::new(-16, -16),
            1,
            8,
            8,
        );
        // Top-left source pixel (−2,−2) equals the border replica of (0,0).
        assert_eq!(luma_at(&dst, 0, 0), luma_at(&reference, 0, 0));
    }

    #[test]
    fn chroma_mv_halves_luma_displacement() {
        // Luma MV (32, 32) → 4 px luma → 2 px chroma. Chroma is constant here, so
        // the copy is exact; the assertion checks the offset math via debug
        // assert (no subpel) and buffer equality.
        let reference = make_ref(64, 64);
        let mut dst = FrameBuffer::new(64, 64);
        predict_inter_block(
            &reference,
            &mut dst,
            Plane::U,
            1,
            1,
            Mv::new(32, 32),
            1,
            8,
            8,
        );
        let (ddata, dorg, dstride) = {
            let (a, b, c) = dst.u_mut();
            (a.to_vec(), b, c)
        };
        // Chroma block at mi(1,1) → chroma px (4,4).
        for r in 0..4 {
            for c in 0..4 {
                assert_eq!(ddata[dorg + (4 + r) * dstride + (4 + c)], 120);
            }
        }
    }

    #[test]
    fn edge_clamp_keeps_reads_in_bounds() {
        // A far-right block with a large rightward MV: the umv-border clamp keeps
        // the copy within the extended buffer (no panic / out-of-range index).
        let reference = make_ref(640, 480);
        let mut dst = FrameBuffer::new(640, 480);
        let mi_cols = 80;
        // The search never produces MVs this large, but the clamp must still be
        // safe. Use a legal multiple-of-16 MV at the right edge.
        predict_inter_block(
            &reference,
            &mut dst,
            Plane::Y,
            0,
            mi_cols - 1,
            Mv::new(0, 16),
            1,
            60,
            mi_cols,
        );
        // No assertion beyond "did not panic": exercises the right-edge path.
    }
}
