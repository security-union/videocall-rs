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

//! Coefficient tokenization: `vp9/encoder/vp9_tokenize.c` `tokenize_b`.
//!
//! Turns a quantized transform block into the coefficient token stream the
//! bitstream packer emits. Each [`Token`] carries the three unconstrained model
//! probabilities of the coefficient position it was formed at (`coef_probs[band]
//! [ctx]`), so the packer ([`super::pack`]) never has to recompute contexts —
//! exactly mirroring libvpx's `TOKENEXTRA::context_tree`.
//!
//! The token/extra-bit split of a coefficient magnitude is computed
//! arithmetically ([`get_token_extra`]) rather than via libvpx's giant
//! `dct_cat_lt_10_value_tokens` lookup; a unit test proves equivalence across
//! the whole representable range.

use crate::vp9::common::block::TxSize;
use crate::vp9::common::generated::{
    COEFBAND_TRANS_4X4, COEFBAND_TRANS_8X8PLUS, DEFAULT_COEF_PROBS_4X4, DEFAULT_COEF_PROBS_8X8,
    NEIGHBORS_DEFAULT_4X4, NEIGHBORS_DEFAULT_8X8, PT_ENERGY_CLASS, SCAN_DEFAULT_4X4,
    SCAN_DEFAULT_8X8,
};

/// Coefficient token alphabet (`vp9/common/vp9_entropy.h`).
pub const ZERO_TOKEN: u8 = 0;
pub const ONE_TOKEN: u8 = 1;
pub const TWO_TOKEN: u8 = 2;
pub const CATEGORY1_TOKEN: u8 = 5;
pub const CATEGORY6_TOKEN: u8 = 10;
pub const EOB_TOKEN: u8 = 11;

/// `CAT6_MIN_VAL`: smallest magnitude coded with the CATEGORY6 token.
const CAT6_MIN_VAL: i32 = 67;

/// Plane type for coefficient probability selection (`PLANE_TYPE`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaneType {
    Y = 0,
    Uv = 1,
}

/// One emitted coefficient token, ready for the packer.
///
/// `probs` is `coef_probs[tx_size][plane_type][INTRA][band][ctx]` — the three
/// unconstrained nodes (EOB, ZERO, ONE/pivot). `extra` is the libvpx `EXTRABIT`:
/// for value tokens it is `(offset << 1) | sign`.
#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub token: u8,
    pub probs: [u8; 3],
    pub extra: i32,
}

/// `vp9_get_token_extra`: map a signed quantized coefficient to its token and
/// extra-bit payload. Computed arithmetically; equivalent to the libvpx lookup
/// table over the whole representable range (see tests).
pub fn get_token_extra(v: i32) -> (u8, i32) {
    let a = v.abs();
    if a >= CAT6_MIN_VAL {
        // extra = 2*(|v| - CAT6_MIN_VAL) with the sign folded into bit 0.
        let extra = if v >= 0 {
            2 * v - 2 * CAT6_MIN_VAL
        } else {
            -2 * v - 2 * CAT6_MIN_VAL + 1
        };
        return (CATEGORY6_TOKEN, extra);
    }
    let sign = (v < 0) as i32;
    // (token, base value) by magnitude band. ONE..FOUR have base == magnitude
    // (zero offset); CAT1..CAT5 carry `magnitude - base` in their extra bits.
    let (token, base) = match a {
        1 => (ONE_TOKEN, 1),
        2 => (2, 2),
        3 => (3, 3),
        4 => (4, 4),
        5..=6 => (5, 5),
        7..=10 => (6, 7),
        11..=18 => (7, 11),
        19..=34 => (8, 19),
        35..=66 => (9, 35),
        _ => unreachable!("get_token_extra called with zero coefficient"),
    };
    let extra = ((a - base) << 1) | sign;
    (token, extra)
}

/// `vp9_pt_energy_class[token]`.
fn energy_class(token: u8) -> u8 {
    PT_ENERGY_CLASS[token as usize]
}

/// `get_coef_context`: `(1 + energy[nb0] + energy[nb1]) >> 1` at scan index `c`.
#[inline]
fn coef_context(neighbors: &[i16], token_cache: &[u8], c: usize) -> usize {
    let n0 = neighbors[2 * c] as usize;
    let n1 = neighbors[2 * c + 1] as usize;
    ((1 + token_cache[n0] as usize + token_cache[n1] as usize) >> 1).min(5)
}

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
        _ => unreachable!("tokenizer only supports 4x4 and 8x8 transforms"),
    }
}

/// `coef_probs[tx_size][plane_type][INTRA][band][ctx]` (the three unconstrained
/// nodes). `INTRA` (ref type 0) is the only reference type on a keyframe.
fn coef_probs(tx_size: TxSize, plane: PlaneType, band: usize, ctx: usize) -> [u8; 3] {
    let p = plane as usize;
    match tx_size {
        TxSize::Tx4X4 => DEFAULT_COEF_PROBS_4X4[p][0][band][ctx],
        TxSize::Tx8X8 => DEFAULT_COEF_PROBS_8X8[p][0][band][ctx],
        _ => unreachable!("tokenizer only supports 4x4 and 8x8 transforms"),
    }
}

/// Tokenize one transform block. `qcoeff` is the raster-order quantized block,
/// `eob` the end-of-block index (count of scan positions up to the last nonzero).
/// `pt` is the initial coefficient context (`get_entropy_context` of the DC
/// position). Returns the token list; the caller updates entropy contexts from
/// `eob > 0`.
///
/// Port of `tokenize_b` (minus backward-adaptation counters, which the
/// error-resilient encoder never uses).
pub fn tokenize_block(
    qcoeff: &[i16],
    eob: usize,
    tx_size: TxSize,
    plane: PlaneType,
    pt_init: usize,
) -> Vec<Token> {
    let (scan, neighbors, band, tx_eob) = tables(tx_size);
    let mut token_cache = [0u8; 64];
    let mut tokens = Vec::new();
    let mut pt = pt_init;
    let mut c = 0usize;

    while c < eob {
        let mut v = qcoeff[scan[c] as usize] as i32;
        // Run of zero tokens up to the next nonzero coefficient.
        while v == 0 {
            tokens.push(Token {
                token: ZERO_TOKEN,
                probs: coef_probs(tx_size, plane, band[c] as usize, pt),
                extra: 0,
            });
            token_cache[scan[c] as usize] = 0;
            c += 1;
            pt = coef_context(neighbors, &token_cache, c);
            v = qcoeff[scan[c] as usize] as i32;
        }
        let (token, extra) = get_token_extra(v);
        tokens.push(Token {
            token,
            probs: coef_probs(tx_size, plane, band[c] as usize, pt),
            extra,
        });
        token_cache[scan[c] as usize] = energy_class(token);
        c += 1;
        pt = coef_context(neighbors, &token_cache, c);
    }

    if c < tx_eob {
        tokens.push(Token {
            token: EOB_TOKEN,
            probs: coef_probs(tx_size, plane, band[c] as usize, pt),
            extra: 0,
        });
    }

    tokens
}

/// `combine_entropy_contexts(a, b)` = `(a != 0) + (b != 0)`.
#[inline]
pub fn combine_entropy_contexts(a: bool, b: bool) -> usize {
    a as usize + b as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder's reconstruction of a coefficient from a (token, extra) pair:
    /// base value for the token's category plus the offset carried in the extra
    /// bits, with bit 0 as the sign. Mirrors `vp9/decoder/vp9_detokenize.c`.
    fn decode_value(token: u8, extra: i32) -> i32 {
        // Category base magnitudes: CAT1_MIN..CAT6_MIN.
        const BASE: [i32; 11] = [0, 1, 2, 3, 4, 5, 7, 11, 19, 35, 67];
        let mag = BASE[token as usize] + (extra >> 1);
        if extra & 1 != 0 {
            -mag
        } else {
            mag
        }
    }

    #[test]
    fn token_extra_round_trips_through_decoder() {
        // For every representable magnitude, the encoder's (token, extra) must
        // reconstruct to exactly the original value the decoder will read back.
        let vmax = 67 + (1 << 14) - 1;
        for a in 1..=vmax {
            for &v in &[a, -a] {
                let (token, extra) = get_token_extra(v);
                assert_eq!(decode_value(token, extra), v, "value {v}");
            }
        }
    }

    #[test]
    fn token_category_boundaries() {
        // Token category assignment at each magnitude boundary.
        for (v, want) in [
            (1, ONE_TOKEN),
            (4, 4),
            (5, 5),
            (6, 5),
            (7, 6),
            (10, 6),
            (11, 7),
            (18, 7),
            (19, 8),
            (34, 8),
            (35, 9),
            (66, 9),
            (67, CATEGORY6_TOKEN),
        ] {
            assert_eq!(get_token_extra(v).0, want, "value {v}");
        }
    }

    #[test]
    fn token_extra_cat6_boundary_and_range() {
        // Just below CAT6: CAT5 (token 9). At/above: CAT6 (token 10).
        assert_eq!(get_token_extra(66).0, 9);
        assert_eq!(get_token_extra(-66).0, 9);
        assert_eq!(get_token_extra(67).0, CATEGORY6_TOKEN);
        assert_eq!(get_token_extra(-67).0, CATEGORY6_TOKEN);
        // CAT6 offset/sign packing: extra = (|v|-67)<<1 | sign.
        assert_eq!(get_token_extra(67), (CATEGORY6_TOKEN, 0));
        assert_eq!(get_token_extra(-67), (CATEGORY6_TOKEN, 1));
        assert_eq!(get_token_extra(68), (CATEGORY6_TOKEN, 2));
        assert_eq!(get_token_extra(-68), (CATEGORY6_TOKEN, 3));
        // Largest codable magnitude (CAT6_MAX_ABS from the quantizer clamp).
        let vmax = 67 + (1 << 14) - 1;
        let (t, e) = get_token_extra(vmax);
        assert_eq!(t, CATEGORY6_TOKEN);
        assert_eq!(e, (vmax - 67) << 1);
    }

    #[test]
    fn all_zero_block_emits_single_eob() {
        let q = [0i16; 64];
        let toks = tokenize_block(&q, 0, TxSize::Tx8X8, PlaneType::Y, 0);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].token, EOB_TOKEN);
    }

    #[test]
    fn dc_only_block_emits_one_then_eob() {
        let mut q = [0i16; 16];
        q[0] = 1; // DC = +1 ⇒ ONE_TOKEN, positive sign.
        let toks = tokenize_block(&q, 1, TxSize::Tx4X4, PlaneType::Y, 0);
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].token, ONE_TOKEN);
        assert_eq!(toks[0].extra & 1, 0);
        assert_eq!(toks[1].token, EOB_TOKEN);
    }

    #[test]
    fn full_block_has_no_eob_token() {
        // A block whose last scan position is nonzero (eob == tx_eob) omits the
        // EOB token, matching the decoder's implicit end-of-block.
        let mut q = [0i16; 16];
        for (i, c) in q.iter_mut().enumerate() {
            *c = if i % 2 == 0 { 1 } else { -1 };
        }
        let toks = tokenize_block(&q, 16, TxSize::Tx4X4, PlaneType::Y, 0);
        assert!(toks.iter().all(|t| t.token != EOB_TOKEN));
        assert_eq!(toks.len(), 16);
    }
}
