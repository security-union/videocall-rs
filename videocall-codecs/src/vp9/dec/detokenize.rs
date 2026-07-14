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

//! Coefficient token decoding: `vp9/decoder/vp9_detokenize.c` `decode_coefs`.
//!
//! The exact inverse of the encoder's [`crate::vp9::enc::tokenize`] +
//! `pack_mb_tokens`: walks a transform block in scan order, reading the EOB /
//! ZERO / ONE model nodes and the constrained Pareto subtree from the boolean
//! reader, then dequantizes each coefficient into a raster-order block ready for
//! the inverse transform. Produces exactly the `dqcoeff` the encoder fed to its
//! `idct*_add`, so the reconstruction is bit-identical.

use crate::vp9::common::block::TxSize;
use crate::vp9::common::bool_coder::BoolReader;
use crate::vp9::common::generated::{
    CAT1_PROB, CAT2_PROB, CAT3_PROB, CAT4_PROB, CAT5_PROB, CAT6_PROB, COEFBAND_TRANS_4X4,
    COEFBAND_TRANS_8X8PLUS, DEFAULT_COEF_PROBS_4X4, DEFAULT_COEF_PROBS_8X8, NEIGHBORS_DEFAULT_4X4,
    NEIGHBORS_DEFAULT_8X8, PARETO8_FULL, PT_ENERGY_CLASS, SCAN_DEFAULT_4X4, SCAN_DEFAULT_8X8,
};
use crate::vp9::common::trees::{read_tree, COEF_CON_TREE};

/// Plane type for coefficient probability selection (`PLANE_TYPE`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaneType {
    Y = 0,
    Uv = 1,
}

/// Reference type index into `coef_probs`: intra blocks (all of a keyframe).
pub const REF_TYPE_INTRA: usize = 0;
/// Reference type index into `coef_probs`: inter blocks.
#[allow(dead_code)] // consumed by the M1 inter decoder
pub const REF_TYPE_INTER: usize = 1;

/// Scan / neighbor / band tables and the coefficient count for a transform size
/// (default scan only — DC_PRED intra ⇒ DCT_DCT, and all chroma).
fn tables(tx_size: TxSize) -> (&'static [i16], &'static [i16], &'static [u8], usize) {
    match tx_size {
        TxSize::Tx4X4 => (
            &SCAN_DEFAULT_4X4,
            &NEIGHBORS_DEFAULT_4X4,
            &COEFBAND_TRANS_4X4,
            16,
        ),
        TxSize::Tx8X8 => (
            &SCAN_DEFAULT_8X8,
            &NEIGHBORS_DEFAULT_8X8,
            &COEFBAND_TRANS_8X8PLUS,
            64,
        ),
        _ => unreachable!("detokenizer only supports 4x4 and 8x8 transforms"),
    }
}

/// `coef_probs[tx_size][plane_type][ref_type][band][ctx]` (the three
/// unconstrained model nodes: EOB, ZERO, ONE/pivot).
#[inline]
fn coef_probs(tx_size: TxSize, plane: usize, ref_type: usize, band: usize, ctx: usize) -> [u8; 3] {
    match tx_size {
        TxSize::Tx4X4 => DEFAULT_COEF_PROBS_4X4[plane][ref_type][band][ctx],
        TxSize::Tx8X8 => DEFAULT_COEF_PROBS_8X8[plane][ref_type][band][ctx],
        _ => unreachable!("detokenizer only supports 4x4 and 8x8 transforms"),
    }
}

/// `get_coef_context`: `(1 + energy[nb0] + energy[nb1]) >> 1`, clamped to 5.
#[inline]
fn coef_context(neighbors: &[i16], token_cache: &[u8], c: usize) -> usize {
    let n0 = neighbors[2 * c] as usize;
    let n1 = neighbors[2 * c + 1] as usize;
    ((1 + token_cache[n0] as usize + token_cache[n1] as usize) >> 1).min(5)
}

/// Extra-bit probabilities for CATEGORY1..CATEGORY6 (`vp9_catN_prob`).
fn extra_bit_probs(token: u8) -> &'static [u8] {
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

/// Base magnitude for each coefficient token (`CATn_MIN_VAL`). Index by token
/// value: ONE..FOUR are their own magnitude; CAT1..CAT6 add read extra bits.
const TOKEN_BASE: [i32; 11] = [0, 1, 2, 3, 4, 5, 7, 11, 19, 35, 67];

/// Read the magnitude of a non-zero coefficient given the decoded `token`
/// (`2..=10` from the constrained subtree, or the caller's ONE handling).
fn read_magnitude(r: &mut BoolReader, token: u8) -> i32 {
    let base = TOKEN_BASE[token as usize];
    let probs = extra_bit_probs(token);
    let mut extra = 0i32;
    for &p in probs {
        extra = (extra << 1) | r.read(p) as i32;
    }
    base + extra
}

/// Decode one transform block's coefficients from `r`.
///
/// `ctx0` is the initial coefficient context (`get_entropy_context` of the DC
/// position) and `dequant` the `[DC, AC]` step sizes for this plane. Returns the
/// end-of-block count and the dequantized coefficients in **raster** order (the
/// first `16`/`64` entries are meaningful for a 4x4/8x8 transform).
pub fn decode_coefs(
    r: &mut BoolReader,
    tx_size: TxSize,
    plane: PlaneType,
    ref_type: usize,
    ctx0: usize,
    dequant: [i16; 2],
) -> (usize, [i16; 64]) {
    let (scan, neighbors, band, tx_eob) = tables(tx_size);
    let plane = plane as usize;
    let mut dqcoeff = [0i16; 64];
    let mut token_cache = [0u8; 64];
    let mut ctx = ctx0;
    let mut c = 0usize;

    'outer: loop {
        if c >= tx_eob {
            break;
        }
        let mut probs = coef_probs(tx_size, plane, ref_type, band[c] as usize, ctx);

        // EOB model node: a 0 bit ends the block (implicit trailing zeros).
        if r.read(probs[0]) == 0 {
            break;
        }

        // Zero run: each 0 at the ZERO node is a zero coefficient; a 1 stops it.
        while r.read(probs[1]) == 0 {
            token_cache[scan[c] as usize] = 0;
            c += 1;
            if c >= tx_eob {
                break 'outer;
            }
            ctx = coef_context(neighbors, &token_cache, c);
            probs = coef_probs(tx_size, plane, ref_type, band[c] as usize, ctx);
        }

        // Non-zero coefficient at position c, magnitude via ONE node then the
        // constrained subtree keyed on the Pareto tail of the pivot probability.
        let (token, mag) = if r.read(probs[2]) == 0 {
            (1u8, 1i32) // ONE_TOKEN
        } else {
            // A valid stream only reaches the pivot node at bands/contexts with a
            // real (non-zero) prob model; the `[0,0,0]` placeholder entries in the
            // coef-prob tables are never selected, so `probs[2] - 1` never
            // underflows. Assert the invariant rather than mask a desync.
            debug_assert!(
                probs[2] > 0,
                "coef pivot probability is a [0,0,0] placeholder (context desync)"
            );
            let pareto = &PARETO8_FULL[(probs[2] - 1) as usize];
            let token = read_tree(r, &COEF_CON_TREE, pareto) as u8; // 2..=10
            (token, read_magnitude(r, token))
        };
        let signed = if r.read_bit() != 0 { -mag } else { mag };
        // DC (scan position 0) uses the DC step; every other position uses AC.
        let dqv = if c == 0 { dequant[0] } else { dequant[1] } as i32;
        dqcoeff[scan[c] as usize] = (signed * dqv) as i16;
        token_cache[scan[c] as usize] = PT_ENERGY_CLASS[token as usize];

        c += 1;
        if c < tx_eob {
            ctx = coef_context(neighbors, &token_cache, c);
        }
    }

    (c, dqcoeff)
}
