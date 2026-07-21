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

//! VP9 token trees and the tree writer/reader.
//!
//! A tree is a flat `&[i8]` (`vpx_tree_index`): each node is a pair of entries.
//! A non-positive entry `-v` is a leaf with value `v`; a positive entry is the
//! array index of a child node. Probabilities are indexed by `node >> 1`.
//!
//! [`write_tree`]/[`write_token`] port libvpx `vp9_write_tree`/`vp9_write_token`
//! (`vp9/encoder/vp9_treewriter.h`); [`read_tree`] ports `vpx_read_tree`
//! (`vpx_dsp/bitreader.h`). Token (value,len) pairs are derived at runtime from
//! the tree with [`tree_to_tokens`], a port of `tree2tok`.
//!
//! All tree constants are the resolved integer forms of the libvpx definitions
//! (enum symbols substituted for their numeric values). The source array and the
//! enum values are cited alongside each constant.

use super::bool_coder::{BoolReader, BoolWriter};

/// A flat token tree, as in libvpx (`vpx_tree_index[]`).
pub type Tree = [i8];

/// Encode `token` of `tree` using `probs`. Port of `vp9_write_token`.
pub fn write_token(w: &mut BoolWriter, tree: &Tree, probs: &[u8], token: usize) {
    let (value, len) = token_value_len(tree, token);
    write_tree(w, tree, probs, value, len, 0);
}

/// Encode the `len` low bits of `bits` walking `tree` from node `start`.
/// Port of `vp9_write_tree`.
pub fn write_tree(w: &mut BoolWriter, tree: &Tree, probs: &[u8], bits: u32, len: u32, start: i8) {
    let mut i = start;
    let mut remaining = len;
    loop {
        remaining -= 1;
        let bit = ((bits >> remaining) & 1) as u8;
        w.write(bit, probs[(i >> 1) as usize]);
        i = tree[(i + bit as i8) as usize];
        if remaining == 0 {
            break;
        }
    }
}

/// Decode one token from `tree` using `probs`. Port of `vpx_read_tree`.
pub fn read_tree(r: &mut BoolReader, tree: &Tree, probs: &[u8]) -> usize {
    let mut i: i8 = 0;
    loop {
        let bit = r.read(probs[(i >> 1) as usize]);
        i = tree[(i + bit as i8) as usize];
        if i <= 0 {
            return (-i) as usize;
        }
    }
}

/// Compute the (value, len) code for a single `token`. Convenience wrapper over
/// [`tree_to_tokens`] for callers that need just one token.
pub fn token_value_len(tree: &Tree, token: usize) -> (u32, u32) {
    let tokens = tree_to_tokens(tree);
    tokens[token]
}

/// Derive the `(value, len)` bit code for every leaf of `tree`. Port of
/// `vp9_tokens_from_tree` / `tree2tok`. The returned vector is indexed by leaf
/// value; unreachable indices are `(0, 0)`.
pub fn tree_to_tokens(tree: &Tree) -> Vec<(u32, u32)> {
    // Leaf values (`-entry` for non-positive entries) are not always a dense
    // 0..n range (e.g. the coef tree's leaves are token values 2..=10), so size
    // the table by the largest leaf value present.
    let max_leaf = tree
        .iter()
        .filter(|&&e| e <= 0)
        .map(|&e| (-e) as usize)
        .max()
        .unwrap_or(0);
    let mut tokens = vec![(0u32, 0u32); max_leaf + 1];
    tree2tok(tree, 0, 0, 0, &mut tokens);
    tokens
}

fn tree2tok(tree: &Tree, i: usize, v: u32, l: u32, tokens: &mut [(u32, u32)]) {
    let mut v = v * 2;
    let l = l + 1;
    let mut idx = i;
    loop {
        let j = tree[idx];
        idx += 1;
        if j <= 0 {
            tokens[(-j) as usize] = (v, l);
        } else {
            tree2tok(tree, j as usize, v, l, tokens);
        }
        v += 1;
        if v & 1 == 0 {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tree constants (resolved integer forms of the libvpx definitions).
// ---------------------------------------------------------------------------

/// `vp9_intra_mode_tree` (`vp9/common/vp9_entropymode.c`). Leaves are
/// PREDICTION_MODE values DC_PRED=0 … TM_PRED=9.
#[rustfmt::skip]
pub const INTRA_MODE_TREE: [i8; 18] = [
    0,  2,   // -DC_PRED, 2
    -9, 4,   // -TM_PRED, 4
    -1, 6,   // -V_PRED, 6
    8,  12,  // COM_NODE
    -2, 10,  // -H_PRED, 10
    -4, -5,  // -D135_PRED, -D117_PRED
    -3, 14,  // -D45_PRED, 14
    -8, 16,  // -D63_PRED, 16
    -6, -7,  // -D153_PRED, -D207_PRED
];

/// `vp9_inter_mode_tree` (`vp9/common/vp9_entropymode.c`). Leaves are
/// `INTER_OFFSET(mode)`: ZEROMV→2, NEARESTMV→0, NEARMV→1, NEWMV→3.
#[rustfmt::skip]
pub const INTER_MODE_TREE: [i8; 6] = [
    -2, 2,   // -INTER_OFFSET(ZEROMV), 2
     0, 4,   // -INTER_OFFSET(NEARESTMV), 4
    -1, -3,  // -INTER_OFFSET(NEARMV), -INTER_OFFSET(NEWMV)
];

/// `vp9_partition_tree` (`vp9/common/vp9_entropymode.c`). Leaves are PARTITION
/// types NONE=0, HORZ=1, VERT=2, SPLIT=3.
#[rustfmt::skip]
pub const PARTITION_TREE: [i8; 6] = [
    0, 2, -1, 4, -2, -3,
];

/// `vp9_switchable_interp_tree` (`vp9/common/vp9_entropymode.c`). Leaves are
/// filter types EIGHTTAP=0, EIGHTTAP_SMOOTH=1, EIGHTTAP_SHARP=2.
#[rustfmt::skip]
pub const SWITCHABLE_INTERP_TREE: [i8; 4] = [
    0, 2, -1, -2,
];

/// `vp9_mv_joint_tree` (`vp9/common/vp9_entropymv.c`). Leaves ZERO=0, HNZVZ=1,
/// HZVNZ=2, HNZVNZ=3.
#[rustfmt::skip]
pub const MV_JOINT_TREE: [i8; 6] = [
    0, 2, -1, 4, -2, -3,
];

/// `vp9_mv_class_tree` (`vp9/common/vp9_entropymv.c`). Leaves MV_CLASS_0..10.
#[rustfmt::skip]
pub const MV_CLASS_TREE: [i8; 20] = [
    0,  2,  -1, 4,  6,  8,  -2, -3, 10,  12,
    -4, -5, -6, 14, 16, 18, -7, -8, -9, -10,
];

/// `vp9_mv_class0_tree` (`vp9/common/vp9_entropymv.c`). Leaves 0,1.
#[rustfmt::skip]
pub const MV_CLASS0_TREE: [i8; 2] = [
    0, -1,
];

/// `vp9_mv_fp_tree` (`vp9/common/vp9_entropymv.c`). Leaves 0..3.
#[rustfmt::skip]
pub const MV_FP_TREE: [i8; 6] = [
    0, 2, -1, 4, -2, -3,
];

/// `vp9_coef_con_tree` (`vp9/common/vp9_entropy.c`): the constrained coefficient
/// subtree. Leaves are token values TWO=2 … CATEGORY6=10 (the EOB/ZERO/ONE nodes
/// are handled by the coefficient model, not this tree). 9 leaves ⇒ 16 entries.
#[rustfmt::skip]
pub const COEF_CON_TREE: [i8; 16] = [
    2,  6,        // 0 = LOW_VAL
    -2, 4,        // 1 = TWO:   -TWO_TOKEN, 4
    -3, -4,       // 2 = THREE: -THREE_TOKEN, -FOUR_TOKEN
    8,  10,       // 3 = HIGH_LOW
    -5, -6,       // 4 = CAT_ONE:   -CATEGORY1_TOKEN, -CATEGORY2_TOKEN
    12, 14,       // 5 = CAT_THREEFOUR
    -7, -8,       // 6 = CAT_THREE:  -CATEGORY3_TOKEN, -CATEGORY4_TOKEN
    -9, -10,      // 7 = CAT_FIVE:   -CATEGORY5_TOKEN, -CATEGORY6_TOKEN
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a set of leaf tokens through the tree writer and reader.
    fn round_trip(tree: &Tree, tokens: &[usize]) {
        // A varied-but-valid prob table (one prob per internal node).
        let n_probs = tree.len() / 2;
        let probs: Vec<u8> = (0..n_probs)
            .map(|i| ((i * 37 + 11) % 254 + 1) as u8)
            .collect();

        let mut w = BoolWriter::new();
        for &t in tokens {
            write_token(&mut w, tree, &probs, t);
        }
        let bytes = w.finalize();
        let mut r = BoolReader::new(&bytes);
        for &t in tokens {
            assert_eq!(read_tree(&mut r, tree, &probs), t, "token {t} mismatch");
        }
    }

    #[test]
    fn intra_mode_tree_all_leaves() {
        let all: Vec<usize> = (0..=9).collect();
        round_trip(&INTRA_MODE_TREE, &all);
        // Repeated / interleaved sequence too.
        round_trip(&INTRA_MODE_TREE, &[0, 9, 0, 1, 2, 8, 0, 3, 4, 5, 6, 7, 0]);
    }

    #[test]
    fn inter_mode_tree_all_leaves() {
        round_trip(&INTER_MODE_TREE, &[0, 1, 2, 3, 2, 2, 0, 3]);
    }

    #[test]
    fn partition_tree_all_leaves() {
        round_trip(&PARTITION_TREE, &[0, 1, 2, 3, 0, 0, 3, 1]);
    }

    #[test]
    fn switchable_interp_tree_all_leaves() {
        round_trip(&SWITCHABLE_INTERP_TREE, &[0, 1, 2, 0, 0]);
    }

    #[test]
    fn mv_joint_tree_all_leaves() {
        round_trip(&MV_JOINT_TREE, &[0, 1, 2, 3]);
    }

    #[test]
    fn mv_class_tree_all_leaves() {
        let all: Vec<usize> = (0..=10).collect();
        round_trip(&MV_CLASS_TREE, &all);
    }

    #[test]
    fn mv_class0_tree_all_leaves() {
        round_trip(&MV_CLASS0_TREE, &[0, 1, 0, 1, 1]);
    }

    #[test]
    fn mv_fp_tree_all_leaves() {
        round_trip(&MV_FP_TREE, &[0, 1, 2, 3]);
    }

    #[test]
    fn coef_con_tree_all_leaves() {
        // Leaves are token values 2..=10.
        let all: Vec<usize> = (2..=10).collect();
        round_trip(&COEF_CON_TREE, &all);
    }

    #[test]
    fn token_len_matches_expected_depths() {
        // In the intra mode tree, DC_PRED is the shallowest leaf (depth 1).
        let toks = tree_to_tokens(&INTRA_MODE_TREE);
        assert_eq!(toks[0], (0, 1)); // DC_PRED: first branch, bit 0
                                     // TM_PRED is reached via bit 1 then bit 0 => value 0b10, len 2.
        assert_eq!(toks[9], (0b10, 2));
    }
}
