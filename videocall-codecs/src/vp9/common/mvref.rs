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

//! Spatial motion-vector reference derivation: `vp9/common/vp9_mvref_common.{c,h}`.
//!
//! Ports the error-resilient (spatial-only, no temporal `prev_frame_mvs`) path of
//! `find_mv_refs_idx` for whole-block (`block == -1`) prediction: it scans the
//! `mv_ref_blocks` neighborhood, collects up to two distinct candidate MVs whose
//! reference frame matches, derives the inter-mode probability context from the
//! two nearest neighbors' modes, then clamps and lowers the candidates exactly as
//! the decoder does before it uses them (`clamp_mv_ref` then `lower_mv_precision`,
//! matching `dec_find_mv_refs` + `read_inter_block_mode_info`).
//!
//! The decoder recomputes `mode_context` and the nearest reference MV from the
//! same neighbor grid, so any disagreement here corrupts every inter frame. The
//! returned `nearest` (index 0) is what the decoder reconstructs as
//! `best_ref_mvs[0]` for NEARESTMV/NEWMV (`early_break` collapses its candidate
//! list to the first match); `near` (index 1) is provided for completeness and
//! the NEARMV mode this encoder does not emit.

use crate::vp9::common::block::{BlockSize, NUM_8X8_BLOCKS_HIGH, NUM_8X8_BLOCKS_WIDE};

/// A motion vector in 1/8-pel units (`MV`). `row`/`col` mirror libvpx.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Mv {
    pub row: i16,
    pub col: i16,
}

impl Mv {
    pub const ZERO: Mv = Mv { row: 0, col: 0 };

    #[inline]
    pub fn new(row: i16, col: i16) -> Self {
        Mv { row, col }
    }
}

// --- Reference-frame identifiers (MV_REFERENCE_FRAME) ------------------------

/// `NONE` (no reference).
pub const NONE_FRAME: i8 = -1;
/// `INTRA_FRAME`.
pub const INTRA_FRAME: i8 = 0;
/// `LAST_FRAME` — the only inter reference this encoder uses.
pub const LAST_FRAME: i8 = 1;

/// `MAX_MV_REF_CANDIDATES`.
pub const MAX_MV_REF_CANDIDATES: usize = 2;
/// `MVREF_NEIGHBOURS`.
const MVREF_NEIGHBOURS: usize = 8;
/// `MV_BORDER` — 16 pels in 1/8-pel units (`clamp_mv_ref`).
const MV_BORDER: i32 = 16 << 3;
/// `MI_SIZE` in pixels.
const MI_SIZE: i32 = 8;

/// Per-neighbor mode/reference/motion info the search reads from the mi grid
/// (a slim projection of the full `MODE_INFO`).
#[derive(Clone, Copy, Debug)]
pub struct MvRefInfo {
    /// `mode` — a [`crate::vp9::common::block::PredictionMode`] discriminant.
    pub mode: u8,
    /// `ref_frame[0..2]` (`[0]` primary, `[1]` = [`NONE_FRAME`] for single ref).
    pub ref_frame: [i8; 2],
    /// `mv[0..2]`.
    pub mv: [Mv; 2],
}

impl MvRefInfo {
    /// An inter block using `LAST_FRAME` with a single reference and MV `mv`.
    pub fn inter_last(mode: u8, mv: Mv) -> Self {
        MvRefInfo {
            mode,
            ref_frame: [LAST_FRAME, NONE_FRAME],
            mv: [mv, Mv::ZERO],
        }
    }

    /// An intra block (`INTRA_FRAME`, no MV).
    pub fn intra(mode: u8) -> Self {
        MvRefInfo {
            mode,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            mv: [Mv::ZERO, Mv::ZERO],
        }
    }

    #[inline]
    fn is_inter(&self) -> bool {
        self.ref_frame[0] > INTRA_FRAME
    }

    #[inline]
    fn has_second_ref(&self) -> bool {
        self.ref_frame[1] > INTRA_FRAME
    }
}

/// `mode_2_counter[MB_MODE_COUNT]` (`vp9_mvref_common.h`): intra 9, NEARESTMV 0,
/// NEARMV 0, ZEROMV 3, NEWMV 1.
const MODE_2_COUNTER: [i32; 14] = [9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 0, 0, 3, 1];

/// `counter_to_context[19]` mapping the flattened neighbor mode counter to an
/// inter-mode context (`INVALID_CASE` = 9 never indexes a probability model).
const COUNTER_TO_CONTEXT: [u8; 19] = [
    2, // BOTH_PREDICTED
    3, // NEW_PLUS_NON_INTRA
    4, // BOTH_NEW
    1, // ZERO_PLUS_PREDICTED
    3, // NEW_PLUS_NON_INTRA
    9, // INVALID_CASE
    0, // BOTH_ZERO
    9, 9, // INVALID_CASE
    5, 5, // INTRA_PLUS_NON_INTRA
    9, // INVALID_CASE
    5, // INTRA_PLUS_NON_INTRA
    9, 9, 9, 9, 9, // INVALID_CASE
    6, // BOTH_INTRA
];

/// `mv_ref_blocks[BLOCK_SIZES][MVREF_NEIGHBOURS]` as `(row, col)` offsets.
#[rustfmt::skip]
const MV_REF_BLOCKS: [[(i32, i32); MVREF_NEIGHBOURS]; 13] = [
    // 4X4
    [(-1, 0), (0, -1), (-1, -1), (-2, 0), (0, -2), (-2, -1), (-1, -2), (-2, -2)],
    // 4X8
    [(-1, 0), (0, -1), (-1, -1), (-2, 0), (0, -2), (-2, -1), (-1, -2), (-2, -2)],
    // 8X4
    [(-1, 0), (0, -1), (-1, -1), (-2, 0), (0, -2), (-2, -1), (-1, -2), (-2, -2)],
    // 8X8
    [(-1, 0), (0, -1), (-1, -1), (-2, 0), (0, -2), (-2, -1), (-1, -2), (-2, -2)],
    // 8X16
    [(0, -1), (-1, 0), (1, -1), (-1, -1), (0, -2), (-2, 0), (-2, -1), (-1, -2)],
    // 16X8
    [(-1, 0), (0, -1), (-1, 1), (-1, -1), (-2, 0), (0, -2), (-1, -2), (-2, -1)],
    // 16X16
    [(-1, 0), (0, -1), (-1, 1), (1, -1), (-1, -1), (-3, 0), (0, -3), (-3, -3)],
    // 16X32
    [(0, -1), (-1, 0), (2, -1), (-1, -1), (-1, 1), (0, -3), (-3, 0), (-3, -3)],
    // 32X16
    [(-1, 0), (0, -1), (-1, 2), (-1, -1), (1, -1), (-3, 0), (0, -3), (-3, -3)],
    // 32X32
    [(-1, 1), (1, -1), (-1, 2), (2, -1), (-1, -1), (-3, 0), (0, -3), (-3, -3)],
    // 32X64
    [(0, -1), (-1, 0), (4, -1), (-1, 2), (-1, -1), (0, -3), (-3, 0), (2, -1)],
    // 64X32
    [(-1, 0), (0, -1), (-1, 4), (2, -1), (-1, -1), (-3, 0), (0, -3), (-1, 2)],
    // 64X64
    [(-1, 3), (3, -1), (-1, 4), (4, -1), (-1, -1), (-1, 0), (0, -1), (-1, 6)],
];

/// Frame geometry and reference configuration for [`find_mv_refs`].
#[derive(Clone, Copy)]
pub struct MvRefGeom {
    pub mi_rows: i32,
    pub mi_cols: i32,
    /// The reference frame being searched for (`LAST_FRAME` here).
    pub ref_frame: i8,
    /// `ref_frame_sign_bias[MAX_REF_FRAMES]`.
    pub ref_sign_bias: [i32; 4],
    /// `allow_high_precision_mv`.
    pub allow_hp: bool,
}

#[inline]
fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

/// `clamp_mv_ref`: clamp `mv` to the block's edges expanded by [`MV_BORDER`].
fn clamp_mv_ref(mv: &mut Mv, mi_row: i32, mi_col: i32, bsize: BlockSize, geom: &MvRefGeom) {
    let bw = NUM_8X8_BLOCKS_WIDE[bsize as usize] as i32;
    let bh = NUM_8X8_BLOCKS_HIGH[bsize as usize] as i32;
    let mb_to_top = -(mi_row * MI_SIZE) * 8;
    let mb_to_bottom = (geom.mi_rows - bh - mi_row) * MI_SIZE * 8;
    let mb_to_left = -(mi_col * MI_SIZE) * 8;
    let mb_to_right = (geom.mi_cols - bw - mi_col) * MI_SIZE * 8;
    mv.col = clamp(
        mv.col as i32,
        mb_to_left - MV_BORDER,
        mb_to_right + MV_BORDER,
    ) as i16;
    mv.row = clamp(
        mv.row as i32,
        mb_to_top - MV_BORDER,
        mb_to_bottom + MV_BORDER,
    ) as i16;
}

/// `lower_mv_precision(mv, allow_hp)`. With `allow_hp = false` (or a ref that is
/// not high-precision eligible) both components are forced even.
pub fn lower_mv_precision(mv: &mut Mv, allow_hp: bool) {
    let use_hp = allow_hp && use_mv_hp(mv);
    if !use_hp {
        if mv.row & 1 != 0 {
            mv.row += if mv.row > 0 { -1 } else { 1 };
        }
        if mv.col & 1 != 0 {
            mv.col += if mv.col > 0 { -1 } else { 1 };
        }
    }
}

/// `use_mv_hp(ref)` — high precision only for small reference MVs.
#[inline]
pub fn use_mv_hp(r: &Mv) -> bool {
    const THRESH: i16 = 64;
    r.row.abs() < THRESH && r.col.abs() < THRESH
}

/// Add `mv` to the candidate list with libvpx's dedupe semantics
/// (`ADD_MV_REF_LIST`). Returns `true` when the list is full (caller stops).
#[inline]
fn add_mv_ref(mv: Mv, list: &mut [Mv; MAX_MV_REF_CANDIDATES], count: &mut usize) -> bool {
    if *count != 0 {
        if mv != list[0] {
            list[1] = mv;
            *count = 2;
            return true;
        }
    } else {
        list[0] = mv;
        *count = 1;
    }
    false
}

/// `vp9_find_mv_refs` (spatial-only, whole-block) followed by the decoder's
/// clamp + precision-lowering of the candidates it uses.
///
/// `neighbor(row, col)` returns the mi at absolute grid position `(row, col)` if
/// it is a valid candidate, else `None` — this is libvpx's `is_inside` folded
/// into the lookup. Callers bound the rows to the frame (`0 <= row < mi_rows`)
/// and the columns to the current tile (`tile_col_start <= col < tile_col_end`),
/// which for a single-tile frame is the whole width (`0 <= col < mi_cols`).
///
/// Returns `(mode_context, [nearest, near])`.
pub fn find_mv_refs<F>(
    neighbor: F,
    mi_row: i32,
    mi_col: i32,
    bsize: BlockSize,
    geom: &MvRefGeom,
) -> (u8, [Mv; MAX_MV_REF_CANDIDATES])
where
    F: Fn(i32, i32) -> Option<MvRefInfo>,
{
    let search = &MV_REF_BLOCKS[bsize as usize];
    let mut list = [Mv::ZERO; MAX_MV_REF_CANDIDATES];
    let mut count = 0usize;
    let mut context_counter = 0i32;
    let mut different_ref_found = false;
    let ref_frame = geom.ref_frame;

    // Nearest two neighbors also drive the mode context.
    let mut done = false;
    let mut i = 0;
    while i < 2 {
        let (dr, dc) = search[i];
        if let Some(c) = neighbor(mi_row + dr, mi_col + dc) {
            context_counter += MODE_2_COUNTER[c.mode as usize];
            different_ref_found = true;
            if c.ref_frame[0] == ref_frame {
                if add_mv_ref(c.mv[0], &mut list, &mut count) {
                    done = true;
                    break;
                }
            } else if c.ref_frame[1] == ref_frame && add_mv_ref(c.mv[1], &mut list, &mut count) {
                done = true;
                break;
            }
        }
        i += 1;
    }

    // Remaining neighbors: same matching, no mode counting.
    if !done {
        while i < MVREF_NEIGHBOURS {
            let (dr, dc) = search[i];
            if let Some(c) = neighbor(mi_row + dr, mi_col + dc) {
                different_ref_found = true;
                if c.ref_frame[0] == ref_frame {
                    if add_mv_ref(c.mv[0], &mut list, &mut count) {
                        done = true;
                        break;
                    }
                } else if c.ref_frame[1] == ref_frame && add_mv_ref(c.mv[1], &mut list, &mut count)
                {
                    done = true;
                    break;
                }
            }
            i += 1;
        }
    }

    // Second pass: motion vectors from a different reference frame.
    if !done && different_ref_found {
        for &(dr, dc) in search.iter() {
            if let Some(c) = neighbor(mi_row + dr, mi_col + dc) {
                if if_diff_ref_frame_add_mv(
                    &c,
                    ref_frame,
                    &geom.ref_sign_bias,
                    &mut list,
                    &mut count,
                ) {
                    break;
                }
            }
        }
    }

    let mode_context = COUNTER_TO_CONTEXT[context_counter as usize];

    // Clamp both candidates (dec_find_mv_refs Done loop), then lower precision on
    // the candidate the decoder actually uses (read_inter_block_mode_info).
    for mv in list.iter_mut() {
        clamp_mv_ref(mv, mi_row, mi_col, bsize, geom);
        lower_mv_precision(mv, geom.allow_hp);
    }

    (mode_context, list)
}

/// `IF_DIFF_REF_FRAME_ADD_MV`: for an inter neighbor, add each of its MVs whose
/// reference frame differs from `ref_frame` (with sign-bias inversion). Returns
/// `true` when the list becomes full.
fn if_diff_ref_frame_add_mv(
    c: &MvRefInfo,
    ref_frame: i8,
    sign_bias: &[i32; 4],
    list: &mut [Mv; MAX_MV_REF_CANDIDATES],
    count: &mut usize,
) -> bool {
    if !c.is_inter() {
        return false;
    }
    if c.ref_frame[0] != ref_frame && add_mv_ref(scale_mv(c, 0, ref_frame, sign_bias), list, count)
    {
        return true;
    }
    if c.has_second_ref()
        && c.ref_frame[1] != ref_frame
        && c.mv[1] != c.mv[0]
        && add_mv_ref(scale_mv(c, 1, ref_frame, sign_bias), list, count)
    {
        return true;
    }
    false
}

/// `scale_mv`: invert the MV sign when the candidate and target reference frames
/// have opposite sign bias.
fn scale_mv(c: &MvRefInfo, r: usize, this_ref: i8, sign_bias: &[i32; 4]) -> Mv {
    let mut mv = c.mv[r];
    if sign_bias[c.ref_frame[r] as usize] != sign_bias[this_ref as usize] {
        mv.row = -mv.row;
        mv.col = -mv.col;
    }
    mv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::block::PredictionMode;

    const NEARESTMV: u8 = PredictionMode::NearestMv as u8; // 10
    const ZEROMV: u8 = PredictionMode::ZeroMv as u8; // 12
    const NEWMV: u8 = PredictionMode::NewMv as u8; // 13
    const DC: u8 = PredictionMode::DcPred as u8; // 0

    fn geom() -> MvRefGeom {
        MvRefGeom {
            mi_rows: 60,
            mi_cols: 80,
            ref_frame: LAST_FRAME,
            ref_sign_bias: [0; 4],
            allow_hp: false,
        }
    }

    /// Build a neighbor closure from an explicit `(row, col) -> MvRefInfo` map.
    fn grid(
        cells: Vec<((i32, i32), MvRefInfo)>,
        rows: i32,
        cols: i32,
    ) -> impl Fn(i32, i32) -> Option<MvRefInfo> {
        move |r: i32, c: i32| {
            if r < 0 || c < 0 || r >= rows || c >= cols {
                return None;
            }
            cells
                .iter()
                .find(|((rr, cc), _)| *rr == r && *cc == c)
                .map(|(_, mi)| *mi)
        }
    }

    // mode_context is counter_to_context[sum of mode_2_counter over the two
    // nearest neighbors: mv_ref_blocks[8X8][0]={-1,0}(above),
    // {0,-1}(left)]. counter_to_context defined in vp9_mvref_common.h:67.

    #[test]
    fn no_neighbors_frame_corner() {
        // (0,0): both nearest neighbors out of frame → counter 0 → BOTH_PREDICTED
        // (counter_to_context[0] = 2). No candidates.
        let (ctx, list) = find_mv_refs(grid(vec![], 60, 80), 0, 0, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 2);
        assert_eq!(list, [Mv::ZERO, Mv::ZERO]);
    }

    #[test]
    fn both_zeromv_neighbors() {
        // above + left both ZEROMV: counter 3+3=6 → counter_to_context[6]=0 (BOTH_ZERO).
        let cells = vec![
            ((0, 1), MvRefInfo::inter_last(ZEROMV, Mv::ZERO)),
            ((1, 0), MvRefInfo::inter_last(ZEROMV, Mv::ZERO)),
        ];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 0);
        // Both candidates are the same ZERO MV; dedupe keeps one, near stays 0.
        assert_eq!(list, [Mv::ZERO, Mv::ZERO]);
    }

    #[test]
    fn above_only_newmv() {
        // above NEWMV (counter 1), left absent → counter 1 → counter_to_context[1]=3.
        let mv = Mv::new(16, -16);
        let cells = vec![((0, 1), MvRefInfo::inter_last(NEWMV, mv))];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 3);
        assert_eq!(list[0], mv); // small MV: clamp + lower are no-ops.
        assert_eq!(list[1], Mv::ZERO);
    }

    #[test]
    fn left_only_nearestmv() {
        // left NEARESTMV (counter 0), above absent → counter 0 → BOTH_PREDICTED (2).
        let mv = Mv::new(-24, 8);
        let cells = vec![((1, 0), MvRefInfo::inter_last(NEARESTMV, mv))];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 2);
        assert_eq!(list[0], mv);
    }

    #[test]
    fn two_distinct_mvs_from_both_neighbors() {
        // above NEWMV(1) + left NEWMV(1) → counter 2 → BOTH_NEW (counter_to_context[2]=4).
        let a = Mv::new(16, 0);
        let l = Mv::new(0, -16);
        let cells = vec![
            ((0, 1), MvRefInfo::inter_last(NEWMV, a)),
            ((1, 0), MvRefInfo::inter_last(NEWMV, l)),
        ];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 4);
        // above is scanned first (mv_ref_blocks[0]={-1,0}), so it is nearest.
        assert_eq!(list[0], a);
        assert_eq!(list[1], l);
    }

    #[test]
    fn dedupe_identical_mvs() {
        let mv = Mv::new(32, -16);
        let cells = vec![
            ((0, 1), MvRefInfo::inter_last(NEWMV, mv)),
            ((1, 0), MvRefInfo::inter_last(NEWMV, mv)),
        ];
        let (_ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(list[0], mv);
        assert_eq!(list[1], Mv::ZERO); // identical → not added twice.
    }

    #[test]
    fn intra_neighbor_contributes_context_but_no_mv() {
        // above intra (counter 9), left NEWMV (counter 1) → counter 10 →
        // counter_to_context[10]=5 (INTRA_PLUS_NON_INTRA).
        let l = Mv::new(-16, -16);
        let cells = vec![
            ((0, 1), MvRefInfo::intra(DC)),
            ((1, 0), MvRefInfo::inter_last(NEWMV, l)),
        ];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 5);
        assert_eq!(list[0], l);
    }

    #[test]
    fn both_intra_neighbors() {
        // counter 9+9=18 → counter_to_context[18]=6 (BOTH_INTRA).
        let cells = vec![
            ((0, 1), MvRefInfo::intra(DC)),
            ((1, 0), MvRefInfo::intra(DC)),
        ];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 6);
        assert_eq!(list, [Mv::ZERO, Mv::ZERO]);
    }

    #[test]
    fn zero_plus_predicted_context() {
        // above ZEROMV(3) + left NEARESTMV(0) → counter 3 →
        // counter_to_context[3]=1 (ZERO_PLUS_PREDICTED).
        let cells = vec![
            ((0, 1), MvRefInfo::inter_last(ZEROMV, Mv::ZERO)),
            ((1, 0), MvRefInfo::inter_last(NEARESTMV, Mv::new(8, 8))),
        ];
        let (ctx, _list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 1);
    }

    #[test]
    fn new_plus_non_intra_context() {
        // above NEWMV(1) + left NEARESTMV(0) → counter 1 →
        // counter_to_context[1]=3 (NEW_PLUS_NON_INTRA).
        let cells = vec![
            ((0, 1), MvRefInfo::inter_last(NEWMV, Mv::new(16, 16))),
            ((1, 0), MvRefInfo::inter_last(NEARESTMV, Mv::new(16, 16))),
        ];
        let (ctx, _list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 3);
    }

    #[test]
    fn nearest_taken_from_farther_neighbor_when_nearest_two_miss() {
        // Nearest two neighbors absent; a farther neighbor (mv_ref_blocks[8X8][2]
        // = {-1,-1}) supplies the only candidate. Its mode does not affect the
        // context (counter stays 0 → BOTH_PREDICTED=2).
        let mv = Mv::new(-32, 48);
        let cells = vec![((0, 0), MvRefInfo::inter_last(NEWMV, mv))];
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        assert_eq!(ctx, 2);
        assert_eq!(list[0], mv);
        assert_eq!(list[1], Mv::ZERO);
    }

    #[test]
    fn odd_candidate_is_lowered_to_even() {
        // A neighbor MV with odd components is forced even by lower_mv_precision
        // (allow_hp = false). Values stay within clamp range so only the
        // precision step changes them.
        let cells = vec![((0, 1), MvRefInfo::inter_last(NEWMV, Mv::new(7, -13)))];
        let (_ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &geom());
        // 7 -> 6 (odd, positive → -1); -13 -> -12 (odd, negative → +1).
        assert_eq!(list[0], Mv::new(6, -12));
    }

    #[test]
    fn candidate_is_clamped_to_border_at_left_edge() {
        // At mi_col 0 a large leftward neighbor MV is clamped to
        // mb_to_left_edge - MV_BORDER = 0 - 128 = -128.
        let cells = vec![((1, 0), MvRefInfo::inter_last(NEWMV, Mv::new(0, -4000)))];
        let g = MvRefGeom {
            mi_cols: 80,
            ..geom()
        };
        // Neighbor is left of (1,1); place current at (1,0)? Use (1,1) with a
        // left neighbor at (1,0).
        let (_ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &g);
        // current mi_col = 1: mb_to_left = -(1*8*8) = -64, min col = -64 - 128 = -192.
        assert_eq!(list[0].col, -192);
    }

    #[test]
    fn candidate_clamped_at_right_edge() {
        // Current block at the far right; a large rightward MV clamps to
        // mb_to_right_edge + MV_BORDER.
        let mi_cols = 80;
        let cur_col = mi_cols - 1; // 79
        let cells = vec![(
            (1, cur_col - 1),
            MvRefInfo::inter_last(NEWMV, Mv::new(0, 4000)),
        )];
        let (_ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, cur_col, BlockSize::B8X8, &geom());
        // mb_to_right = (80 - 1 - 79)*8*8 = 0; max col = 0 + 128 = 128.
        assert_eq!(list[0].col, 128);
    }

    #[test]
    fn different_ref_second_pass_with_sign_bias() {
        // The only neighbor uses a different ref (GOLDEN=2) with opposite sign
        // bias, so its MV is added in the second pass, sign-inverted. Context
        // counter comes from its mode (NEWMV → 1 for the nearest slot).
        let golden = MvRefInfo {
            mode: NEWMV,
            ref_frame: [2, NONE_FRAME],
            mv: [Mv::new(16, -32), Mv::ZERO],
        };
        let cells = vec![((0, 1), golden)];
        let g = MvRefGeom {
            ref_sign_bias: [0, 0, 1, 0], // GOLDEN(2) biased opposite to LAST(1).
            ..geom()
        };
        let (ctx, list) = find_mv_refs(grid(cells, 60, 80), 1, 1, BlockSize::B8X8, &g);
        assert_eq!(ctx, 3); // NEW_PLUS_NON_INTRA (counter 1).
        assert_eq!(list[0], Mv::new(-16, 32)); // sign inverted.
    }
}
