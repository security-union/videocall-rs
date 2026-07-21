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

//! Inter prediction-context derivations: `vp9/common/vp9_pred_common.{c,h}`.
//!
//! `get_intra_inter_context` and `vp9_get_pred_context_single_ref_p1` derive the
//! probability context for the per-block `is_inter` and single-reference flags
//! from the two coded neighbors (`above_mi` / `left_mi`). They are
//! decoder-mandated: the encoder packs each flag under the context computed here,
//! and the decoder MUST recompute the identical context to read it back, so a
//! single definition lives in `common` and both directions call it.

use crate::vp9::common::mvref::{INTRA_FRAME, LAST_FRAME};

/// A neighbor mode-info block's fields consumed by the inter prediction-context
/// derivations (`above_mi` / `left_mi`). `None` at a frame/tile edge, mirroring
/// libvpx's dummy `NULL` neighbor.
#[derive(Clone, Copy, Debug)]
pub struct InterNeighbor {
    pub is_inter: bool,
    /// `ref_frame[0]`.
    pub ref0: i8,
    /// `ref_frame[1]` (`NONE` for single reference).
    pub ref1: i8,
}

impl InterNeighbor {
    #[inline]
    fn has_second_ref(&self) -> bool {
        self.ref1 > INTRA_FRAME
    }
}

/// `get_intra_inter_context` (`vp9_pred_common.h`).
pub fn intra_inter_context(above: Option<InterNeighbor>, left: Option<InterNeighbor>) -> usize {
    match (above, left) {
        (Some(a), Some(l)) => {
            let above_intra = !a.is_inter;
            let left_intra = !l.is_inter;
            if left_intra && above_intra {
                3
            } else {
                (left_intra || above_intra) as usize
            }
        }
        (Some(e), None) | (None, Some(e)) => 2 * (!e.is_inter) as usize,
        (None, None) => 0,
    }
}

/// `vp9_get_pred_context_single_ref_p1` (`vp9_pred_common.c`), specialized to a
/// single reference (no second ref) but ported faithfully across the neighbor
/// combinations.
pub fn single_ref_p1_context(above: Option<InterNeighbor>, left: Option<InterNeighbor>) -> usize {
    let is_last = |m: &InterNeighbor| (m.ref0 == LAST_FRAME) as usize;
    match (above, left) {
        (Some(a), Some(l)) => {
            let above_intra = !a.is_inter;
            let left_intra = !l.is_inter;
            if above_intra && left_intra {
                2
            } else if above_intra || left_intra {
                let e = if above_intra { &l } else { &a };
                if !e.has_second_ref() {
                    4 * is_last(e)
                } else {
                    1 + ((e.ref0 == LAST_FRAME || e.ref1 == LAST_FRAME) as usize)
                }
            } else {
                let above_second = a.has_second_ref();
                let left_second = l.has_second_ref();
                if above_second && left_second {
                    1 + ((a.ref0 == LAST_FRAME
                        || a.ref1 == LAST_FRAME
                        || l.ref0 == LAST_FRAME
                        || l.ref1 == LAST_FRAME) as usize)
                } else if above_second || left_second {
                    let rfs = if !above_second { a.ref0 } else { l.ref0 };
                    let (crf1, crf2) = if above_second {
                        (a.ref0, a.ref1)
                    } else {
                        (l.ref0, l.ref1)
                    };
                    if rfs == LAST_FRAME {
                        3 + ((crf1 == LAST_FRAME || crf2 == LAST_FRAME) as usize)
                    } else {
                        (crf1 == LAST_FRAME || crf2 == LAST_FRAME) as usize
                    }
                } else {
                    2 * (a.ref0 == LAST_FRAME) as usize + 2 * (l.ref0 == LAST_FRAME) as usize
                }
            }
        }
        (Some(e), None) | (None, Some(e)) => {
            if !e.is_inter {
                2
            } else if !e.has_second_ref() {
                4 * is_last(&e)
            } else {
                1 + ((e.ref0 == LAST_FRAME || e.ref1 == LAST_FRAME) as usize)
            }
        }
        (None, None) => 2,
    }
}
