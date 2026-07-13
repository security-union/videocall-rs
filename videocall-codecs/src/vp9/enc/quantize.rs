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

//! Forward quantization (fast-path / `fp`).
//!
//! Ports the realtime quantizer libvpx uses under the `--rt` deadline:
//! `vp9_quantize_fp_c` (`vp9/encoder/vp9_quantize.c`) together with the
//! per-qindex `quant_fp`/`round_fp` derivation from `vp9_init_quantizer`. The
//! dequant step sizes come from [`super::super::common::quant`].
//!
//! Coefficients are visited in scan order so the returned end-of-block index
//! matches the decoder's. Quantized magnitudes are clamped to the range the
//! coefficient token machinery can encode (CAT6), and the dequantized output is
//! computed from the clamped value so encoder and decoder reconstruct
//! identically.

use crate::vp9::common::block::TxSize;
use crate::vp9::common::generated::{SCAN_DEFAULT_4X4, SCAN_DEFAULT_8X8};
use crate::vp9::common::quant::{ac_quant, dc_quant};

/// Largest coefficient magnitude the token coder can represent (8-bit): the
/// CAT6 base value (67) plus its 14 extra bits (`2^14 - 1`). Larger quantized
/// values are clamped here so every emitted coefficient is codable.
pub const CAT6_MAX_ABS: i32 = 67 + (1 << 14) - 1;

/// Per-qindex quantizer factors for one plane, index `[0] = DC`, `[1] = AC`.
#[derive(Clone, Copy, Debug)]
pub struct QuantParams {
    /// Dequant step sizes (`vp9_dc_quant`/`vp9_ac_quant`).
    pub dequant: [i16; 2],
    /// `quant_fp = (1 << 16) / dequant`.
    pub quant_fp: [i16; 2],
    /// `round_fp = (rounding_factor * dequant) >> 7`.
    pub round_fp: [i16; 2],
}

impl QuantParams {
    /// Build the fast-path factors for `qindex` (0..=255), with per-plane DC/AC
    /// deltas (0 for the keyframe Y and, in our configuration, chroma).
    pub fn new(qindex: i32, dc_delta: i32, ac_delta: i32) -> Self {
        let dequant = [dc_quant(qindex, dc_delta), ac_quant(qindex, ac_delta)];
        let mut quant_fp = [0i16; 2];
        let mut round_fp = [0i16; 2];
        for i in 0..2 {
            let d = dequant[i] as i32;
            quant_fp[i] = ((1 << 16) / d) as i16;
            let rounding_factor = if qindex == 0 {
                64
            } else if i == 0 {
                48
            } else {
                42
            };
            round_fp[i] = ((rounding_factor * d) >> 7) as i16;
        }
        Self {
            dequant,
            quant_fp,
            round_fp,
        }
    }
}

/// Number of coefficients and the default scan for a transform size.
fn scan_for(tx_size: TxSize) -> &'static [i16] {
    match tx_size {
        TxSize::Tx4X4 => &SCAN_DEFAULT_4X4,
        TxSize::Tx8X8 => &SCAN_DEFAULT_8X8,
        // Larger transforms are not produced by the current keyframe pipeline.
        _ => unreachable!("quantize_fp only supports 4x4 and 8x8 transforms"),
    }
}

/// Quantize `coeff` (raster order) into `qcoeff` and `dqcoeff`, returning the
/// end-of-block count (number of coefficients up to and including the last
/// non-zero one in scan order). All three slices must be at least `n²` long for
/// the given transform.
///
/// Port of `vp9_quantize_fp_c` with an added CAT6 magnitude clamp.
pub fn quantize_fp(
    coeff: &[i16],
    tx_size: TxSize,
    qp: &QuantParams,
    qcoeff: &mut [i16],
    dqcoeff: &mut [i16],
) -> u16 {
    let scan = scan_for(tx_size);
    let n = scan.len();
    qcoeff[..n].fill(0);
    dqcoeff[..n].fill(0);

    let mut eob: i32 = -1;
    for (i, &rc16) in scan.iter().enumerate() {
        let rc = rc16 as usize;
        let coeff_i = coeff[rc] as i32;
        let sign = coeff_i >> 31; // 0 for non-negative, -1 for negative
        let abs_coeff = (coeff_i ^ sign) - sign;
        let ac = (rc != 0) as usize;

        let mut tmp = (abs_coeff + qp.round_fp[ac] as i32).clamp(i16::MIN as i32, i16::MAX as i32);
        tmp = (tmp * qp.quant_fp[ac] as i32) >> 16;
        tmp = tmp.min(CAT6_MAX_ABS);

        let q = (tmp ^ sign) - sign;
        qcoeff[rc] = q as i16;
        // Decoder computes dqcoeff = qcoeff * dequant truncated to int16.
        dqcoeff[rc] = (q * qp.dequant[ac] as i32) as i16;

        if tmp != 0 {
            eob = i as i32;
        }
    }
    (eob + 1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};
    use crate::vp9::enc::fdct::{fdct4x4, fdct8x8};

    #[test]
    fn quant_params_sane() {
        // qindex 0: smallest step, DC round factor 64.
        let qp = QuantParams::new(0, 0, 0);
        assert_eq!(qp.dequant, [4, 4]);
        assert_eq!(qp.quant_fp, [16384, 16384]);
        assert_eq!(qp.round_fp[0], (64 * 4) >> 7);
        // Higher qindex: larger step, AC round factor 42.
        let qp = QuantParams::new(128, 0, 0);
        assert_eq!(qp.round_fp[1], ((42 * qp.dequant[1] as i32) >> 7) as i16);
    }

    #[test]
    fn all_zero_coeffs_give_eob_zero() {
        let coeff = [0i16; 16];
        let qp = QuantParams::new(100, 0, 0);
        let mut q = [0i16; 16];
        let mut dq = [0i16; 16];
        let eob = quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
        assert_eq!(eob, 0);
        assert!(q.iter().all(|&c| c == 0));
    }

    #[test]
    fn dc_only_gives_eob_one() {
        let mut coeff = [0i16; 16];
        coeff[0] = 2000;
        let qp = QuantParams::new(100, 0, 0);
        let mut q = [0i16; 16];
        let mut dq = [0i16; 16];
        let eob = quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
        assert_eq!(eob, 1);
        assert!(q[0] != 0);
        assert!(q[1..].iter().all(|&c| c == 0));
        // dqcoeff is a multiple of the DC step.
        assert_eq!(dq[0], (q[0] as i32 * qp.dequant[0] as i32) as i16);
    }

    #[test]
    fn dequant_is_multiple_of_step() {
        let mut coeff = [0i16; 64];
        for (i, c) in coeff.iter_mut().enumerate() {
            *c = ((i as i16 * 13) % 97) - 48;
        }
        let qp = QuantParams::new(80, 0, 0);
        let mut q = [0i16; 64];
        let mut dq = [0i16; 64];
        quantize_fp(&coeff, TxSize::Tx8X8, &qp, &mut q, &mut dq);
        for i in 0..64 {
            let ac = (i != 0) as usize;
            assert_eq!(dq[i], (q[i] as i32 * qp.dequant[ac] as i32) as i16);
        }
    }

    /// fdct -> quantize -> idct reconstruction error stays bounded; near-lossless
    /// at qindex 0 and merely bounded at a high qindex.
    #[test]
    fn roundtrip_error_bounded_4x4() {
        let residual: [i16; 16] = [
            40, -12, 33, -7, 18, -25, 9, 14, -30, 6, 21, -3, 11, -19, 5, -8,
        ];
        for (qindex, bound) in [(0i32, 3i32), (100, 40)] {
            let qp = QuantParams::new(qindex, 0, 0);
            let mut coeff = [0i16; 16];
            fdct4x4(&residual, 4, &mut coeff);
            let mut q = [0i16; 16];
            let mut dq = [0i16; 16];
            quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
            let mut recon = [128u8; 16];
            idct4x4_add(&dq, &mut recon, 4);
            for i in 0..16 {
                let want = 128 + residual[i] as i32;
                let got = recon[i] as i32;
                assert!(
                    (want - got).abs() <= bound,
                    "q{qindex} idx{i}: want {want} got {got}"
                );
            }
        }
    }

    #[test]
    fn roundtrip_error_bounded_8x8() {
        let mut residual = [0i16; 64];
        for (i, r) in residual.iter_mut().enumerate() {
            *r = (((i * 29 + 5) % 61) as i16) - 30;
        }
        let qp = QuantParams::new(0, 0, 0);
        let mut coeff = [0i16; 64];
        fdct8x8(&residual, 8, &mut coeff);
        let mut q = [0i16; 64];
        let mut dq = [0i16; 64];
        quantize_fp(&coeff, TxSize::Tx8X8, &qp, &mut q, &mut dq);
        let mut recon = [100u8; 64];
        idct8x8_add(&dq, &mut recon, 8);
        for i in 0..64 {
            let want = 100 + residual[i] as i32;
            let got = recon[i] as i32;
            assert!((want - got).abs() <= 3, "idx{i}: want {want} got {got}");
        }
    }
}
