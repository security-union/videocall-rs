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

//! Encoder-free integer-pel motion search (a simplified stand-in for
//! `vp9/encoder/vp9_mcomp.c`).
//!
//! Searches whole-pixel, **even** luma displacements only — motion vectors are
//! constrained to multiples of 16 in 1/8-pel units so motion compensation stays a
//! pure block copy for both luma and chroma (see
//! [`crate::vp9::common::inter_pred`]). A cheap large-to-small step search around
//! the predicted MV minimizes SAD plus a small motion-cost proxy; being
//! encoder-only, none of this needs to be bit-exact.

use crate::vp9::common::mvref::Mv;

/// Search range in pixels around the start point (even positions only).
const SEARCH_RANGE_PX: i32 = 16;
/// Lambda for the crude `SAD + lambda * mv_bits` cost. Small so SAD dominates.
const LAMBDA: u32 = 4;

/// The reference luma plane and geometry needed to score a displacement.
pub struct RefPlane<'a> {
    pub data: &'a [u8],
    pub origin: usize,
    pub stride: usize,
}

/// The source luma block being matched (an 8x8 region of the current frame).
pub struct SrcBlock<'a> {
    pub data: &'a [u8],
    pub off: usize,
    pub stride: usize,
}

/// Sum of absolute differences between the 8x8 `src` block and the reference at
/// integer pixel offset `(dy, dx)` from the block's mode-info position.
fn sad8x8(reference: &RefPlane, src: &SrcBlock, mi_row: i32, mi_col: i32, dy: i32, dx: i32) -> u32 {
    let base_row = mi_row * 8 + dy;
    let base_col = mi_col * 8 + dx;
    let mut src_p = src.off;
    let mut ref_p =
        (reference.origin as i32 + base_row * reference.stride as i32 + base_col) as usize;
    let mut sad = 0u32;
    for _ in 0..8 {
        for c in 0..8 {
            let s = src.data[src_p + c] as i32;
            let r = reference.data[ref_p + c] as i32;
            sad += (s - r).unsigned_abs();
        }
        src_p += src.stride;
        ref_p += reference.stride;
    }
    sad
}

/// A crude bit-count proxy for a motion vector difference (mimics the log-ish
/// growth of the real MV coder without being exact).
fn mv_cost(mv: Mv, pred: Mv) -> u32 {
    let d = |a: i16, b: i16| {
        let mut v = (a - b).unsigned_abs() as u32;
        let mut bits = 1;
        while v > 0 {
            v >>= 1;
            bits += 1;
        }
        bits
    };
    LAMBDA * (d(mv.row, pred.row) + d(mv.col, pred.col))
}

/// Convert an even pixel offset to a motion vector (1/8-pel, multiple of 16).
#[inline]
fn px_to_mv(dy: i32, dx: i32) -> Mv {
    Mv::new((dy * 8) as i16, (dx * 8) as i16)
}

/// Result of the motion search: the best MV and its SAD (prediction error only).
#[derive(Clone, Copy, Debug)]
pub struct SearchResult {
    pub mv: Mv,
    pub sad: u32,
}

/// Search for the best even whole-pixel MV for one 8x8 luma block.
///
/// `pred_mv` is the predicted (nearest) MV, rounded to the nearest even pixel and
/// used both as the search origin and the cost center. The search always
/// evaluates ZEROMV and the predictor, then refines with a halving step pattern.
/// Returns the winning MV (a multiple of 16 in 1/8-pel) and its SAD.
pub fn search_block(
    reference: &RefPlane,
    src: &SrcBlock,
    mi_row: i32,
    mi_col: i32,
    pred_mv: Mv,
) -> SearchResult {
    // Clamp the search so reads stay inside the reference border (64 px) and the
    // 8x8 block fits: |offset| <= SEARCH_RANGE_PX is well within.
    let clamp_px = |v: i32| v.clamp(-SEARCH_RANGE_PX, SEARCH_RANGE_PX);
    // Round the predictor to an even pixel offset (its px value = mv/8).
    let round_even = |mv8: i16| {
        let px = (mv8 as i32) / 8;
        clamp_px(px & !1)
    };
    let start = (round_even(pred_mv.row), round_even(pred_mv.col));

    let cost_at = |dy: i32, dx: i32| -> (u32, u32) {
        let sad = sad8x8(reference, src, mi_row, mi_col, dy, dx);
        (sad + mv_cost(px_to_mv(dy, dx), pred_mv), sad)
    };

    // Seed with ZEROMV, the predictor, and (0,0)-origin so static content and the
    // predicted motion are always considered.
    let mut best_dy;
    let mut best_dx;
    let mut best_cost;
    let mut best_sad;
    {
        let (c0, s0) = cost_at(0, 0);
        best_dy = 0;
        best_dx = 0;
        best_cost = c0;
        best_sad = s0;
        if start != (0, 0) {
            let (cs, ss) = cost_at(start.0, start.1);
            if cs < best_cost {
                best_dy = start.0;
                best_dx = start.1;
                best_cost = cs;
                best_sad = ss;
            }
        }
    }

    // Large-to-small step search around the current best.
    let mut step = 8;
    while step >= 2 {
        let mut improved = true;
        while improved {
            improved = false;
            const DIRS: [(i32, i32); 8] = [
                (-1, 0),
                (1, 0),
                (0, -1),
                (0, 1),
                (-1, -1),
                (-1, 1),
                (1, -1),
                (1, 1),
            ];
            for (ddy, ddx) in DIRS {
                let dy = clamp_px(best_dy + ddy * step);
                let dx = clamp_px(best_dx + ddx * step);
                if (dy, dx) == (best_dy, best_dx) {
                    continue;
                }
                let (c, s) = cost_at(dy, dx);
                if c < best_cost {
                    best_cost = c;
                    best_sad = s;
                    best_dy = dy;
                    best_dx = dx;
                    improved = true;
                }
            }
        }
        step /= 2;
    }

    SearchResult {
        mv: px_to_mv(best_dy, best_dx),
        sad: best_sad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::frame_buffer::FrameBuffer;

    /// A reference frame with a white box at pixel (bx, by).
    fn boxed_ref(w: u32, h: u32, bx: usize, by: usize, size: usize) -> FrameBuffer {
        let (yw, yh) = (w as usize, h as usize);
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        let mut src = vec![128u8; yw * yh];
        for r in by..(by + size).min(yh) {
            for c in bx..(bx + size).min(yw) {
                src[r * yw + c] = 255;
            }
        }
        src.extend(std::iter::repeat_n(128u8, 2 * cw * ch));
        let mut fb = FrameBuffer::new(w, h);
        fb.import_i420(&src, w, h).unwrap();
        fb.extend_borders();
        fb
    }

    #[test]
    fn finds_translation_of_box() {
        // The box moves down-right by 2 px between frames (as in `moving_box`):
        // reference box at (40, 40), current box at (42, 42). To predict the
        // current block from the reference, the MV points back up-left by 2 px =
        // (-16, -16). The chosen block spans the box's top-left corner so only
        // the true motion yields a perfect match (an interior block would match
        // at many MVs including zero).
        let reference = boxed_ref(128, 128, 40, 40, 16);
        let current = boxed_ref(128, 128, 42, 42, 16);

        let (rdata, rorg, rstride, _, _) = reference.y();
        let (sdata, sorg, sstride, _, _) = current.y();
        let refp = RefPlane {
            data: rdata,
            origin: rorg,
            stride: rstride,
        };
        // Block mi (5,5) → px (40,40): its interior crosses the current box's
        // top-left edge at (42,42).
        let src = SrcBlock {
            data: sdata,
            off: sorg + 40 * sstride + 40,
            stride: sstride,
        };
        let res = search_block(&refp, &src, 5, 5, Mv::ZERO);
        assert_eq!(res.mv, Mv::new(-16, -16), "expected -2px,-2px motion");
        assert_eq!(res.sad, 0, "exact match should have zero SAD");
    }

    #[test]
    fn static_content_prefers_zero_mv() {
        let reference = boxed_ref(128, 128, 40, 40, 16);
        let current = boxed_ref(128, 128, 40, 40, 16);
        let (rdata, rorg, rstride, _, _) = reference.y();
        let (sdata, sorg, sstride, _, _) = current.y();
        let refp = RefPlane {
            data: rdata,
            origin: rorg,
            stride: rstride,
        };
        // A flat background block: SAD is zero at ZEROMV and cost favors it.
        let src = SrcBlock {
            data: sdata,
            off: sorg + 80 * sstride + 80,
            stride: sstride,
        };
        let res = search_block(&refp, &src, 10, 10, Mv::ZERO);
        assert_eq!(res.mv, Mv::ZERO);
        assert_eq!(res.sad, 0);
    }

    #[test]
    fn search_mv_is_multiple_of_16() {
        let reference = boxed_ref(128, 128, 50, 30, 16);
        let current = boxed_ref(128, 128, 44, 36, 16);
        let (rdata, rorg, rstride, _, _) = reference.y();
        let (sdata, sorg, sstride, _, _) = current.y();
        let refp = RefPlane {
            data: rdata,
            origin: rorg,
            stride: rstride,
        };
        let src = SrcBlock {
            data: sdata,
            off: sorg + 36 * sstride + 44,
            stride: sstride,
        };
        let res = search_block(&refp, &src, 4, 5, Mv::ZERO);
        assert_eq!(res.mv.row % 16, 0);
        assert_eq!(res.mv.col % 16, 0);
    }
}
