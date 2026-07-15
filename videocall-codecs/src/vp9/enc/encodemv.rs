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

//! Motion-vector coding: `vp9/encoder/vp9_encodemv.c` `vp9_encode_mv`.
//!
//! Encodes a motion vector as a difference against a reference MV: the joint
//! (which components are non-zero) via `mv_joint_tree`, then per non-zero
//! component the sign, magnitude class (`mv_class_tree`), integer bits, and the
//! fractional (`mv_fp_tree`) sub-tree. The high-precision bit is only written
//! when `usehp` is set, which this encoder never does (`allow_high_precision_mv`
//! is 0), matching the decoder's `read_mv_component` where `hp` defaults to 1.
//!
//! The probability model is the default `nmv_context` (no backward adaptation);
//! the decoder reads it back with the same default tables.

use crate::vp9::common::bool_coder::BoolWriter;
use crate::vp9::common::generated::{
    NMV_BITS_PROBS, NMV_CLASS0_FP_PROBS, NMV_CLASS0_PROBS, NMV_CLASS_PROBS, NMV_FP_PROBS,
    NMV_JOINT_PROBS, NMV_SIGN_PROBS,
};
use crate::vp9::common::mvref::{use_mv_hp, Mv};
use crate::vp9::common::trees::{write_token, MV_CLASS_TREE, MV_FP_TREE, MV_JOINT_TREE};

/// `CLASS0_BITS`.
const CLASS0_BITS: i32 = 1;
/// `log_in_base_2` bound: `CLASS0_SIZE * 4096` (`vp9_get_mv_class`).
const MV_CLASS_MAX_Z: i32 = 2 * 4096;

/// `log_in_base_2[z >> 3]` reproduced as `floor(log2(x))` for `x >= 1`
/// (`log_in_base_2[0] == 0`). Equivalent to the libvpx lookup over its domain.
#[inline]
fn log_in_base_2(x: i32) -> i32 {
    if x <= 0 {
        0
    } else {
        31 - (x as u32).leading_zeros() as i32
    }
}

/// `mv_class_base(c)`.
#[inline]
fn mv_class_base(c: i32) -> i32 {
    if c != 0 {
        2 << (c + 2)
    } else {
        0
    }
}

/// `vp9_get_mv_class(z, &offset)` → `(class, offset)`.
fn get_mv_class(z: i32) -> (i32, i32) {
    let c = if z >= MV_CLASS_MAX_Z {
        10
    } else {
        log_in_base_2(z >> 3)
    };
    (c, z - mv_class_base(c))
}

/// `MV_JOINT_TYPE` for `diff` (`vp9_get_mv_joint`).
#[inline]
fn get_mv_joint(diff: Mv) -> usize {
    if diff.row == 0 {
        if diff.col == 0 {
            0 // MV_JOINT_ZERO
        } else {
            1 // MV_JOINT_HNZVZ
        }
    } else if diff.col == 0 {
        2 // MV_JOINT_HZVNZ
    } else {
        3 // MV_JOINT_HNZVNZ
    }
}

#[inline]
fn mv_joint_vertical(j: usize) -> bool {
    j == 2 || j == 3
}

#[inline]
fn mv_joint_horizontal(j: usize) -> bool {
    j == 1 || j == 3
}

/// `encode_mv_component`: sign, class, integer bits, fractional tree, hp bit.
/// `comp` is a non-zero component difference in 1/8-pel units. `idx` selects the
/// per-component probability set (0 = row/vertical, 1 = col/horizontal).
fn encode_mv_component(w: &mut BoolWriter, comp: i32, idx: usize, usehp: bool) {
    debug_assert!(comp != 0);
    let sign = comp < 0;
    let mag = comp.unsigned_abs() as i32;
    let (mv_class, offset) = get_mv_class(mag - 1);
    let d = offset >> 3; // integer part
    let fr = ((offset >> 1) & 3) as usize; // fractional part
    let hp = offset & 1; // high-precision part

    // Sign.
    w.write(sign as u8, NMV_SIGN_PROBS[idx]);

    // Class.
    write_token(w, &MV_CLASS_TREE, &NMV_CLASS_PROBS[idx], mv_class as usize);

    // Integer bits.
    if mv_class == 0 {
        w.write(d as u8, NMV_CLASS0_PROBS[idx][0]);
    } else {
        let n = mv_class + CLASS0_BITS - 1; // number of bits
        for i in 0..n {
            w.write(((d >> i) & 1) as u8, NMV_BITS_PROBS[idx][i as usize]);
        }
    }

    // Fractional bits.
    let fp_probs: &[u8] = if mv_class == 0 {
        &NMV_CLASS0_FP_PROBS[idx][d as usize]
    } else {
        &NMV_FP_PROBS[idx]
    };
    write_token(w, &MV_FP_TREE, fp_probs, fr);

    // High-precision bit (never emitted while usehp is false).
    if usehp {
        let prob = if mv_class == 0 {
            crate::vp9::common::generated::NMV_CLASS0_HP_PROBS[idx]
        } else {
            crate::vp9::common::generated::NMV_HP_PROBS[idx]
        };
        w.write(hp as u8, prob);
    }
}

/// `vp9_encode_mv`: encode `mv` as a difference from `ref_mv`.
///
/// `usehp` mirrors `allow_high_precision_mv`; the effective high-precision use is
/// gated by `use_mv_hp(ref_mv)` exactly as libvpx does.
pub fn encode_mv(w: &mut BoolWriter, mv: Mv, ref_mv: Mv, usehp: bool) {
    let diff = Mv::new(mv.row - ref_mv.row, mv.col - ref_mv.col);
    let j = get_mv_joint(diff);
    let usehp = usehp && use_mv_hp(&ref_mv);

    write_token(w, &MV_JOINT_TREE, &NMV_JOINT_PROBS, j);
    if mv_joint_vertical(j) {
        encode_mv_component(w, diff.row as i32, 0, usehp);
    }
    if mv_joint_horizontal(j) {
        encode_mv_component(w, diff.col as i32, 1, usehp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::bool_coder::BoolReader;
    use crate::vp9::common::trees::{read_tree, MV_CLASS_TREE, MV_FP_TREE, MV_JOINT_TREE};

    /// A hand-built reader mirroring `read_mv` / `read_mv_component`
    /// (`vp9/decoder/vp9_decodemv.c`) for `usehp = false`.
    fn read_mv(r: &mut BoolReader, ref_mv: Mv) -> Mv {
        let j = read_tree(r, &MV_JOINT_TREE, &NMV_JOINT_PROBS);
        let mut diff = Mv::ZERO;
        if mv_joint_vertical(j) {
            diff.row = read_component(r, 0) as i16;
        }
        if mv_joint_horizontal(j) {
            diff.col = read_component(r, 1) as i16;
        }
        Mv::new(ref_mv.row + diff.row, ref_mv.col + diff.col)
    }

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
        // usehp = false ⇒ hp defaults to 1.
        let hp = 1;
        let mag = mag + ((d << 3) | (fr << 1) | hp) + 1;
        if sign != 0 {
            -mag
        } else {
            mag
        }
    }

    fn round_trip(mv: Mv, ref_mv: Mv) {
        let mut w = BoolWriter::new();
        encode_mv(&mut w, mv, ref_mv, false);
        let bytes = w.finalize();
        let mut r = BoolReader::new(&bytes);
        // With usehp = false the decoder rounds the reconstructed MV to even; our
        // encoder is only ever handed even (multiple-of-16) MVs and even refs, so
        // the recovered value must match exactly.
        assert_eq!(read_mv(&mut r, ref_mv), mv, "mv {mv:?} ref {ref_mv:?}");
    }

    #[test]
    fn zero_diff_round_trips() {
        round_trip(Mv::new(0, 0), Mv::new(0, 0));
        round_trip(Mv::new(16, -16), Mv::new(16, -16));
    }

    #[test]
    fn class_boundaries_round_trip() {
        // Even diffs across every magnitude class boundary (in 1/8-pel units).
        for &d in &[2, 8, 16, 32, 64, 128, 256, 512, 1024, 2048] {
            round_trip(Mv::new(d, 0), Mv::ZERO);
            round_trip(Mv::new(-d, 0), Mv::ZERO);
            round_trip(Mv::new(0, d), Mv::ZERO);
            round_trip(Mv::new(0, -d), Mv::ZERO);
            round_trip(Mv::new(d, -d), Mv::ZERO);
        }
    }

    #[test]
    fn diffs_against_nonzero_ref() {
        let refs = [Mv::new(16, 16), Mv::new(-64, 128), Mv::new(256, -256)];
        for &rm in &refs {
            for &d in &[2, 16, 48, 512] {
                round_trip(Mv::new(rm.row + d, rm.col - d), rm);
            }
        }
    }

    #[test]
    fn mv_class_matches_libvpx_examples() {
        // Spot-check get_mv_class against hand-computed values.
        // z = mag - 1. class = log_in_base_2[z >> 3] (or 10 for z >= 8192).
        // log_in_base_2 begins {0, 0, 1, 1, 2, ...}, so class 0 spans z >> 3 in
        // {0, 1} — i.e. z in [0, 15], magnitudes 1..=16 (up to 2 integer pel).
        assert_eq!(get_mv_class(0).0, 0); // mag 1
        assert_eq!(get_mv_class(15).0, 0); // z>>3 = 1 → log_in_base_2[1] = 0
        assert_eq!(get_mv_class(16).0, 1); // z>>3 = 2 → log_in_base_2[2] = 1
        assert_eq!(get_mv_class(8191).0, 9); // (8191>>3)=1023 → 9
        assert_eq!(get_mv_class(8192).0, 10); // clamped to class 10
    }

    #[test]
    fn log_in_base_2_matches_floor_log2() {
        for x in 1..2048 {
            assert_eq!(log_in_base_2(x), (x as f64).log2().floor() as i32, "x={x}");
        }
    }
}
