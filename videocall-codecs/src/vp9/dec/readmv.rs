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

//! Motion-vector decoding: `vp9/decoder/vp9_decodemv.c` `read_mv` /
//! `read_mv_component`.
//!
//! The exact inverse of the encoder's [`crate::vp9::enc::encodemv`]: reads the
//! joint (`mv_joint_tree`), then per non-zero component the sign, magnitude class
//! (`mv_class_tree`), integer bits, and the fractional (`mv_fp_tree`) sub-tree
//! against the default `nmv_context`, and adds the recovered difference to the
//! reference MV. This encoder never sets `allow_high_precision_mv`, so `usehp` is
//! false and the high-precision bit defaults to 1 — matching
//! `encode_mv(.., usehp = false)`.
//!
//! Component reconstruction runs in `i32` and only the final sum narrows to the
//! `i16` MV field with `wrapping_add`, so a hostile stream coding an out-of-range
//! difference produces a defined (wrapped) value rather than a debug overflow
//! panic; the caller then rejects any non-integer MV before motion compensation.

use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::common::generated::{
    NMV_BITS_PROBS, NMV_CLASS0_FP_PROBS, NMV_CLASS0_PROBS, NMV_CLASS_PROBS, NMV_FP_PROBS,
    NMV_JOINT_PROBS, NMV_SIGN_PROBS,
};
use crate::vp9::common::mvref::Mv;
use crate::vp9::common::trees::{read_tree, MV_CLASS_TREE, MV_FP_TREE, MV_JOINT_TREE};

/// `CLASS0_BITS`.
const CLASS0_BITS: i32 = 1;

#[inline]
fn mv_joint_vertical(j: usize) -> bool {
    j == 2 || j == 3
}

#[inline]
fn mv_joint_horizontal(j: usize) -> bool {
    j == 1 || j == 3
}

/// Read a motion vector as a difference from `ref_mv` (`usehp = false`).
pub fn read_mv(r: &mut BoolReader, ref_mv: Mv) -> Mv {
    let j = read_tree(r, &MV_JOINT_TREE, &NMV_JOINT_PROBS);
    let mut mv = ref_mv;
    if mv_joint_vertical(j) {
        mv.row = ref_mv.row.wrapping_add(read_component(r, 0) as i16);
    }
    if mv_joint_horizontal(j) {
        mv.col = ref_mv.col.wrapping_add(read_component(r, 1) as i16);
    }
    mv
}

/// `read_mv_component` for `usehp = false`. `idx` selects the per-component
/// probability set (0 = row/vertical, 1 = col/horizontal).
fn read_component(r: &mut BoolReader, idx: usize) -> i32 {
    let sign = r.read(NMV_SIGN_PROBS[idx]);
    let mv_class = read_tree(r, &MV_CLASS_TREE, &NMV_CLASS_PROBS[idx]);
    let (d, mag) = if mv_class == 0 {
        (r.read(NMV_CLASS0_PROBS[idx][0]) as i32, 0)
    } else {
        let n = mv_class as i32 + CLASS0_BITS - 1;
        let mut d = 0i32;
        for i in 0..n {
            d |= (r.read(NMV_BITS_PROBS[idx][i as usize]) as i32) << i;
        }
        (d, 2 << (mv_class as i32 + 2))
    };
    let fp_probs: &[u8] = if mv_class == 0 {
        &NMV_CLASS0_FP_PROBS[idx][d as usize]
    } else {
        &NMV_FP_PROBS[idx]
    };
    let fr = read_tree(r, &MV_FP_TREE, fp_probs) as i32;
    // usehp = false ⇒ the high-precision bit defaults to 1.
    let hp = 1;
    let mag = mag + ((d << 3) | (fr << 1) | hp) + 1;
    if sign != 0 {
        -mag
    } else {
        mag
    }
}
