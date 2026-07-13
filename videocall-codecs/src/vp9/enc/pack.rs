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

//! Bitstream packing primitives: `vp9/encoder/vp9_bitstream.c`.
//!
//! Ports `pack_mb_tokens`, `write_skip`, keyframe mode writing, `write_partition`
//! and the partition-context bookkeeping (`partition_plane_context`,
//! `update_partition_context`). The recursive superblock walk that drives these
//! lives in [`super::encoder`].

use crate::vp9::common::block::{
    BlockSize, Partition, B_WIDTH_LOG2, MI_WIDTH_LOG2, NUM_8X8_BLOCKS_WIDE,
};
use crate::vp9::common::bool_coder::BoolWriter;
use crate::vp9::common::generated::{
    DEFAULT_SKIP_PROBS, KF_PARTITION_PROBS, KF_UV_MODE_PROBS, KF_Y_MODE_PROBS, PARETO8_FULL,
};
use crate::vp9::common::trees::{write_token, write_tree, INTRA_MODE_TREE, PARTITION_TREE};
use crate::vp9::enc::tokenize::{Token, CATEGORY1_TOKEN, EOB_TOKEN, ONE_TOKEN, ZERO_TOKEN};

/// `vp9_coef_encodings[token]` = `(value, len)` for the constrained subtree walk
/// (`vp9/encoder/vp9_tokenize.c`). Only tokens `TWO..=CATEGORY6` are used here.
const COEF_ENCODINGS: [(u32, u32); 12] = [
    (2, 2),
    (6, 3),
    (28, 5),
    (58, 6),
    (59, 6),
    (60, 6),
    (61, 6),
    (124, 7),
    (125, 7),
    (126, 7),
    (127, 7),
    (0, 1),
];

/// `PARTITION_PLOFFSET`: probability models per block size in the partition ctx.
const PARTITION_PLOFFSET: usize = 4;
/// `MI_MASK`: superblock-local mi index mask (8 mi per SB64).
const MI_MASK: u32 = 7;

/// Extra-bit probabilities for CATEGORY1..CATEGORY6 (`vp9_catN_prob`, 8-bit).
fn extra_bit_probs(token: u8) -> &'static [u8] {
    use crate::vp9::common::generated::{
        CAT1_PROB, CAT2_PROB, CAT3_PROB, CAT4_PROB, CAT5_PROB, CAT6_PROB,
    };
    match token {
        5 => &CAT1_PROB,
        6 => &CAT2_PROB,
        7 => &CAT3_PROB,
        8 => &CAT4_PROB,
        9 => &CAT5_PROB,
        10 => &CAT6_PROB,
        _ => &[],
    }
}

/// Pack one mode-info block's coefficient tokens (all planes concatenated, in
/// `Y, U, V` order). Verbatim port of `pack_mb_tokens` for a single block (the
/// EOSB delimiter is implicit in the slice boundary).
pub fn pack_mb_tokens(w: &mut BoolWriter, tokens: &[Token]) {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].token == EOB_TOKEN {
            w.write(0, tokens[i].probs[0]);
            i += 1;
            continue;
        }
        // "Not end-of-block" bit, using the first token of this run's context.
        w.write(1, tokens[i].probs[0]);
        // Zero run: each zero writes only the ZERO-node bit.
        while tokens[i].token == ZERO_TOKEN {
            w.write(0, tokens[i].probs[1]);
            i += 1;
        }
        let p = tokens[i];
        // "Not zero" bit.
        w.write(1, p.probs[1]);
        if p.token == ONE_TOKEN {
            w.write(0, p.probs[2]);
            w.write_bit((p.extra & 1) as u8);
        } else {
            // Pivot bit, then the constrained subtree via the Pareto tail.
            w.write(1, p.probs[2]);
            let (value, len) = COEF_ENCODINGS[p.token as usize];
            let pareto = &PARETO8_FULL[(p.probs[2] - 1) as usize];
            write_tree(
                w,
                &crate::vp9::common::trees::COEF_CON_TREE,
                pareto,
                value,
                len - 3,
                0,
            );
            if p.token >= CATEGORY1_TOKEN {
                let probs = extra_bit_probs(p.token);
                let nbits = probs.len();
                let vbits = (p.extra >> 1) as u32;
                for (j, &prob) in probs.iter().enumerate() {
                    let bit = ((vbits >> (nbits - 1 - j)) & 1) as u8;
                    w.write(bit, prob);
                }
            }
            w.write_bit((p.extra & 1) as u8);
        }
        i += 1;
    }
}

/// `write_skip`: emit the block skip flag under its neighbor-derived context.
pub fn write_skip(w: &mut BoolWriter, above_skip: bool, left_skip: bool, skip: bool) {
    let ctx = above_skip as usize + left_skip as usize;
    w.write(skip as u8, DEFAULT_SKIP_PROBS[ctx]);
}

/// Write a keyframe block's mode info: DC_PRED luma (`vp9_kf_y_mode_prob`) then
/// DC_PRED chroma (`vp9_kf_uv_mode_prob`).
///
/// Restricted to DC_PRED everywhere (M1), so the neighbor mode lookup is always
/// `[DC_PRED][DC_PRED]` / `[DC_PRED]` and the mode token is DC_PRED (`0`).
pub fn write_kf_dc_modes(w: &mut BoolWriter) {
    const DC_PRED: usize = 0;
    write_token(
        w,
        &INTRA_MODE_TREE,
        &KF_Y_MODE_PROBS[DC_PRED][DC_PRED],
        DC_PRED,
    );
    write_token(w, &INTRA_MODE_TREE, &KF_UV_MODE_PROBS[DC_PRED], DC_PRED);
}

/// Partition above/left context arrays (`above_seg_context` / `left_seg_context`),
/// mirroring the encoder's per-frame/per-SB-row lifecycle.
pub struct PartitionContext {
    /// One byte per mi column across the (sb-aligned) frame width.
    pub above: Vec<u8>,
    /// One byte per mi row within a superblock (8 entries).
    pub left: [u8; 8],
}

impl PartitionContext {
    pub fn new(mi_cols_aligned: usize) -> Self {
        Self {
            above: vec![0u8; mi_cols_aligned],
            left: [0u8; 8],
        }
    }

    /// Reset the left context at the start of a superblock row.
    pub fn reset_left(&mut self) {
        self.left = [0u8; 8];
    }

    /// `partition_plane_context`: derive the partition probability context.
    fn plane_context(&self, mi_row: u32, mi_col: u32, bsize: BlockSize) -> usize {
        let bsl = MI_WIDTH_LOG2[bsize as usize] as usize;
        let above = ((self.above[mi_col as usize] >> bsl) & 1) as usize;
        let left = ((self.left[(mi_row & MI_MASK) as usize] >> bsl) & 1) as usize;
        (left * 2 + above) + bsl * PARTITION_PLOFFSET
    }

    /// `update_partition_context`: stamp the context after coding a block.
    pub fn update(&mut self, mi_row: u32, mi_col: u32, subsize: BlockSize, bsize: BlockSize) {
        // `partition_context_lookup[subsize]` — the {above, left} masks. For the
        // block sizes this encoder emits, above == left (square blocks).
        const ABOVE: [u8; 13] = [15, 15, 14, 14, 14, 12, 12, 12, 8, 8, 8, 0, 0];
        const LEFT: [u8; 13] = [15, 14, 15, 14, 12, 14, 12, 8, 12, 8, 0, 8, 0];
        let bs = NUM_8X8_BLOCKS_WIDE[bsize as usize] as usize;
        let a = ABOVE[subsize as usize];
        let l = LEFT[subsize as usize];
        for k in 0..bs {
            self.above[mi_col as usize + k] = a;
        }
        let base = (mi_row & MI_MASK) as usize;
        for k in 0..bs {
            self.left[base + k] = l;
        }
    }
}

/// `write_partition`: emit the partition decision with frame-edge handling.
///
/// `hbs` is the half-block size in mi units. At the frame's right/bottom edge one
/// of the partition token's bits is forced (or the whole token dropped), matching
/// `has_rows`/`has_cols` in libvpx.
#[allow(clippy::too_many_arguments)]
pub fn write_partition(
    w: &mut BoolWriter,
    ctx: &PartitionContext,
    hbs: u32,
    mi_row: u32,
    mi_col: u32,
    partition: Partition,
    bsize: BlockSize,
    mi_rows: u32,
    mi_cols: u32,
) {
    let cidx = ctx.plane_context(mi_row, mi_col, bsize);
    let probs = &KF_PARTITION_PROBS[cidx];
    let has_rows = (mi_row + hbs) < mi_rows;
    let has_cols = (mi_col + hbs) < mi_cols;

    if has_rows && has_cols {
        write_token(w, &PARTITION_TREE, probs, partition as usize);
    } else if !has_rows && has_cols {
        // SPLIT or HORZ: one bit distinguishes them.
        w.write((partition == Partition::Split) as u8, probs[1]);
    } else if has_rows && !has_cols {
        // SPLIT or VERT.
        w.write((partition == Partition::Split) as u8, probs[2]);
    }
    // else: neither — partition is forced SPLIT, nothing coded.
}

/// `b_width_log2_lookup[bsize]` — exposed for the walk's `bs = (1<<bsl)/4`.
pub fn b_width_log2(bsize: BlockSize) -> u32 {
    B_WIDTH_LOG2[bsize as usize] as u32
}
