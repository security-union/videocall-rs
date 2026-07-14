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

use crate::vp9::common::block::{BlockSize, B_WIDTH_LOG2};
use crate::vp9::common::bool_coder::BoolWriter;
use crate::vp9::common::generated::{
    DEFAULT_INTER_MODE_PROBS, DEFAULT_INTRA_INTER_PROBS, DEFAULT_SINGLE_REF_PROBS,
    DEFAULT_SKIP_PROBS, KF_UV_MODE_PROBS, KF_Y_MODE_PROBS, PARETO8_FULL,
};
use crate::vp9::common::trees::{write_token, write_tree, INTER_MODE_TREE, INTRA_MODE_TREE};
use crate::vp9::enc::tokenize::{Token, CATEGORY1_TOKEN, EOB_TOKEN, ONE_TOKEN, ZERO_TOKEN};

// The partition-context state and its token reader/writer are decoder-mandated,
// so they live in `common`; re-exported here for the encoder walk that used to
// find them in this module.
pub use crate::vp9::common::partition::{write_partition, PartitionContext};

// The inter prediction-context derivations (`is_inter` / single-ref) are
// decoder-mandated too, so they live in `common`; re-exported here for the
// encoder walk and the writers below that used to find them in this module.
pub use crate::vp9::common::inter_ctx::{
    intra_inter_context, single_ref_p1_context, InterNeighbor,
};

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

/// Write the `is_inter` (`intra_inter`) flag under its neighbor context.
pub fn write_intra_inter(
    w: &mut BoolWriter,
    above: Option<InterNeighbor>,
    left: Option<InterNeighbor>,
    is_inter: bool,
) {
    let ctx = intra_inter_context(above, left);
    w.write(is_inter as u8, DEFAULT_INTRA_INTER_PROBS[ctx]);
}

/// `write_ref_frames` for the single-reference, LAST-only configuration: a single
/// `single_ref_p1` bit of value 0 (LAST). No compound / p2 bits are emitted
/// because `reference_mode == SINGLE_REFERENCE` and the only reference is LAST.
pub fn write_single_ref_last(
    w: &mut BoolWriter,
    above: Option<InterNeighbor>,
    left: Option<InterNeighbor>,
) {
    let ctx = single_ref_p1_context(above, left);
    // bit0 = (ref_frame[0] != LAST_FRAME) = 0.
    w.write(0, DEFAULT_SINGLE_REF_PROBS[ctx][0]);
}

/// `write_inter_mode`: emit the inter-mode token using `inter_mode_probs`
/// indexed by `mode_context`. `inter_offset` is `mode - NEARESTMV`
/// (NEARESTMV→0, NEARMV→1, ZEROMV→2, NEWMV→3).
pub fn write_inter_mode(w: &mut BoolWriter, mode_context: usize, inter_offset: usize) {
    write_token(
        w,
        &INTER_MODE_TREE,
        &DEFAULT_INTER_MODE_PROBS[mode_context],
        inter_offset,
    );
}

/// `b_width_log2_lookup[bsize]` — exposed for the walk's `bs = (1<<bsl)/4`.
pub fn b_width_log2(bsize: BlockSize) -> u32 {
    B_WIDTH_LOG2[bsize as usize] as u32
}
