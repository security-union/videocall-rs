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

//! One-pass VBR rate control.
//!
//! A ~300-line stand-in for `vp9/encoder/vp9_ratectrl.c`'s 3,371 lines, covering
//! the pieces the realtime single-reference encoder needs: a per-frame bit target
//! (average bandwidth plus a bounded drain of the running bit balance, with a
//! keyframe boost), a quantizer chosen by brute-force scan of the app's qindex
//! window, and a per-frame-type correction factor that adapts to how the last
//! frame of that type actually coded.
//!
//! The core bit model — [`bits_per_mb`], [`convert_qindex_to_q`] and
//! [`estimate_bits_at_q`] — is ported faithfully from libvpx so the projections
//! track a real VP9 stream. Everything above it (target derivation, the scan,
//! the correction update) is an encoder-side choice and is deliberately simple.
//!
//! No golden/altref groups, scene-cut detection, frame dropping, or buffer-model
//! CBR: those are out of scope for this encoder.

use crate::encoder::EncoderConfig;
use crate::vp9::common::quant::{ac_quant, quantizer_to_qindex};

/// `BPER_MB_NORMBITS`: bits-per-mb are stored in 1/512-bit fixed point.
const BPER_MB_NORMBITS: u32 = 9;
/// `FRAME_OVERHEAD_BITS`: floor on the estimated size of any frame.
const FRAME_OVERHEAD_BITS: i64 = 200;
/// `MIN_BPB_FACTOR` / `MAX_BPB_FACTOR`: bounds on the rate-correction factor.
const MIN_BPB_FACTOR: f64 = 0.005;
const MAX_BPB_FACTOR: f64 = 50.0;

/// Keyframe bit-target boost over the average per-frame bandwidth.
const KF_BOOST: f64 = 10.0;
/// Divisor applied to the running bit balance when draining it into a frame's
/// target: only a fraction of the surplus/debt is spent (or recovered) per frame
/// so the controller reacts smoothly rather than in one lurch.
const BALANCE_DRAIN_DIVISOR: f64 = 30.0;
/// Per-frame clamp on the correction factor adjustment (`actual/projected`).
const CORRECTION_ADJ_MIN: f64 = 0.9;
const CORRECTION_ADJ_MAX: f64 = 1.1;

/// `vp9_convert_qindex_to_q` (8-bit): the AC dequant step scaled to the legacy Q
/// domain. Ported from `vp9/encoder/vp9_ratectrl.c`.
pub fn convert_qindex_to_q(qindex: i32) -> f64 {
    ac_quant(qindex, 0) as f64 / 4.0
}

/// `vp9_rc_bits_per_mb`: projected bits for one 16x16 macroblock at `qindex`,
/// scaled by `correction_factor`. Ported verbatim (integer truncation included)
/// from `vp9/encoder/vp9_ratectrl.c`; keyframes and inter frames use different
/// enumerators.
pub fn bits_per_mb(is_keyframe: bool, qindex: i32, correction_factor: f64) -> i64 {
    let q = convert_qindex_to_q(qindex);
    let mut enumerator: i64 = if is_keyframe { 2_700_000 } else { 1_800_000 };
    // q-based adjustment to the baseline enumerator (matches libvpx's
    // `(int)(enumerator * q) >> 12`).
    enumerator += ((enumerator as f64 * q) as i64) >> 12;
    (enumerator as f64 * correction_factor / q) as i64
}

/// `vp9_estimate_bits_at_q`: projected size in bits for a whole frame of `mbs`
/// macroblocks at `qindex`, floored at [`FRAME_OVERHEAD_BITS`].
pub fn estimate_bits_at_q(is_keyframe: bool, qindex: i32, mbs: i64, correction_factor: f64) -> i64 {
    let bpm = bits_per_mb(is_keyframe, qindex, correction_factor);
    let est = ((bpm.max(0) as u64 * mbs as u64) >> BPER_MB_NORMBITS) as i64;
    est.max(FRAME_OVERHEAD_BITS)
}

/// One-pass VBR rate controller.
///
/// Owns the target-bitrate/framerate state, the qindex window mapped from the
/// app's 0-63 min/max quantizers, the running bit balance, and the two
/// (keyframe / inter) correction factors. It does **not** own keyframe cadence:
/// the encoder decides frame type and passes it in.
#[derive(Clone, Debug)]
pub struct RateControl {
    /// Target bitrate in bits per second (updatable at runtime).
    target_bps: f64,
    /// Nominal frame rate; the average per-frame budget is `target_bps / fps`.
    fps: f64,
    /// Number of 16x16 macroblocks in a frame (`vp9_set_mb_size`).
    mbs: i64,
    /// Inclusive qindex window `[qmin, qmax]` mapped from the app's min/max q.
    qmin: i32,
    qmax: i32,
    /// Running bit balance: `sum(target - actual)`. Positive = under budget
    /// (credit to spend), negative = over budget (debt to repay).
    bits_balance: f64,
    /// Adaptive correction factors, one per frame type, that scale the bit model
    /// toward what this content actually costs.
    kf_correction: f64,
    inter_correction: f64,
}

impl RateControl {
    /// Build a controller from the encoder configuration. The qindex window comes
    /// from `min_quantizer`/`max_quantizer` via [`quantizer_to_qindex`]; the MB
    /// count from the frame dimensions.
    pub fn new(cfg: &EncoderConfig) -> Self {
        let mut qmin = quantizer_to_qindex(cfg.min_quantizer as i32);
        let mut qmax = quantizer_to_qindex(cfg.max_quantizer as i32);
        if qmin > qmax {
            std::mem::swap(&mut qmin, &mut qmax);
        }
        let mb_cols = (mi_cols(cfg.width) as i64 + 1) >> 1;
        let mb_rows = (mi_rows(cfg.height) as i64 + 1) >> 1;
        Self {
            target_bps: cfg.bitrate_kbps as f64 * 1000.0,
            fps: (cfg.framerate.max(1)) as f64,
            mbs: (mb_cols * mb_rows).max(1),
            qmin,
            qmax,
            bits_balance: 0.0,
            kf_correction: 1.0,
            inter_correction: 1.0,
        }
    }

    /// Inclusive qindex window this controller selects within.
    pub fn qindex_window(&self) -> (i32, i32) {
        (self.qmin, self.qmax)
    }

    /// Average per-frame bit budget (`target_bps / fps`).
    fn avg_frame_bits(&self) -> f64 {
        self.target_bps / self.fps
    }

    /// Target size in bits for the next frame of the given type.
    ///
    /// Inter frames get the average budget plus a bounded drain of the running
    /// balance (`±avg/2`). Keyframes get a fixed boost over the average and do
    /// not drain the balance (a keyframe's large, one-off cost would otherwise
    /// swamp the signal); the balance absorbs the overshoot afterwards.
    pub fn frame_target_bits(&self, is_keyframe: bool) -> i64 {
        let avg = self.avg_frame_bits();
        let target = if is_keyframe {
            avg * KF_BOOST
        } else {
            let drain = (self.bits_balance / BALANCE_DRAIN_DIVISOR).clamp(-avg / 2.0, avg / 2.0);
            avg + drain
        };
        target.max(FRAME_OVERHEAD_BITS as f64) as i64
    }

    /// Choose a qindex for a frame of the given type targeting `target_bits`.
    ///
    /// Scans the window low→high and returns the **lowest** qindex whose
    /// projected size is within budget. Projected size falls monotonically as
    /// qindex rises, so that qindex spends the most bits while staying under
    /// target (best quality within budget). If even `qmax` overshoots, `qmax` is
    /// returned (nothing cheaper is available).
    pub fn select_qindex(&self, is_keyframe: bool, target_bits: i64) -> u8 {
        let cf = self.correction(is_keyframe);
        for q in self.qmin..=self.qmax {
            if estimate_bits_at_q(is_keyframe, q, self.mbs, cf) <= target_bits {
                return q as u8;
            }
        }
        self.qmax as u8
    }

    /// Update the balance and the frame-type correction factor after a frame of
    /// `actual_bits` was coded at `qindex` against `target_bits`.
    ///
    /// The correction factor multiplies by `clamp(actual/projected, 0.9, 1.1)`
    /// each frame, nudging future projections toward reality without letting a
    /// single frame swing it wildly; it is held within libvpx's
    /// `[MIN_BPB_FACTOR, MAX_BPB_FACTOR]` bounds. Keyframes do not move the
    /// balance (their target did not either).
    pub fn update_after_encode(
        &mut self,
        is_keyframe: bool,
        qindex: u8,
        target_bits: i64,
        actual_bits: i64,
    ) {
        let cf = self.correction(is_keyframe);
        let projected = estimate_bits_at_q(is_keyframe, qindex as i32, self.mbs, cf).max(1);
        let adj =
            (actual_bits as f64 / projected as f64).clamp(CORRECTION_ADJ_MIN, CORRECTION_ADJ_MAX);
        let new_cf = (cf * adj).clamp(MIN_BPB_FACTOR, MAX_BPB_FACTOR);
        if is_keyframe {
            self.kf_correction = new_cf;
        } else {
            self.inter_correction = new_cf;
            self.bits_balance += (target_bits - actual_bits) as f64;
        }
    }

    /// Change the target bitrate at runtime.
    ///
    /// The new rate takes effect on the very next frame. The running balance is
    /// **not** zeroed — a large jump would otherwise strand accumulated debt or
    /// credit — but it is damped toward zero so the controller does not keep
    /// chasing the old operating point; the correction factors are left intact
    /// since they describe the content, not the rate.
    pub fn update_bitrate_kbps(&mut self, kbps: u32) {
        self.target_bps = kbps as f64 * 1000.0;
        self.bits_balance *= 0.5;
    }

    /// Current correction factor for the given frame type.
    fn correction(&self, is_keyframe: bool) -> f64 {
        if is_keyframe {
            self.kf_correction
        } else {
            self.inter_correction
        }
    }
}

/// `mi_cols = (width + 7) >> 3` (duplicated from `common::block` to keep this
/// module self-contained; both derive from the same VP9 constant).
fn mi_cols(width: u32) -> u32 {
    (width + 7) >> 3
}

/// `mi_rows = (height + 7) >> 3`.
fn mi_rows(height: u32) -> u32 {
    (height + 7) >> 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bitrate_kbps: u32) -> EncoderConfig {
        EncoderConfig {
            width: 640,
            height: 480,
            framerate: 30,
            bitrate_kbps,
            keyframe_interval: 150,
            min_quantizer: 40,
            max_quantizer: 60,
            cpu_used: 7,
        }
    }

    #[test]
    fn qindex_window_from_min_max_q() {
        let rc = RateControl::new(&cfg(500));
        // q 40 → qindex 160, q 60 → qindex 240 (QUANTIZER_TO_QINDEX table).
        assert_eq!(rc.qindex_window(), (160, 240));
    }

    #[test]
    fn mb_count_640x480() {
        let rc = RateControl::new(&cfg(500));
        // 640x480 → 40x30 macroblocks.
        assert_eq!(rc.mbs, 1200);
    }

    #[test]
    fn bits_per_mb_decreases_with_qindex() {
        // Higher qindex ⇒ fewer projected bits per mb (monotone).
        let mut prev = i64::MAX;
        for q in (0..=255).step_by(8) {
            let b = bits_per_mb(false, q, 1.0);
            assert!(b <= prev, "bpm not monotone at q{q}: {b} > {prev}");
            prev = b;
        }
    }

    #[test]
    fn keyframe_costs_more_than_inter() {
        // Same q, keyframe enumerator (2.7M) > inter (1.8M).
        assert!(bits_per_mb(true, 160, 1.0) > bits_per_mb(false, 160, 1.0));
    }

    #[test]
    fn select_qindex_monotone_in_target() {
        // Higher target ⇒ lower-or-equal chosen qindex.
        let rc = RateControl::new(&cfg(500));
        let mut prev_q = 255u8;
        for &target in &[1_000i64, 5_000, 20_000, 80_000, 400_000, 2_000_000] {
            let q = rc.select_qindex(false, target);
            assert!(
                q <= prev_q,
                "q not monotone: target {target} → q{q} > {prev_q}"
            );
            assert!((rc.qmin as u8..=rc.qmax as u8).contains(&q));
            prev_q = q;
        }
    }

    #[test]
    fn select_qindex_clamps_to_window() {
        let rc = RateControl::new(&cfg(500));
        // Impossibly small target ⇒ qmax; impossibly large ⇒ qmin.
        assert_eq!(rc.select_qindex(false, 1), rc.qmax as u8);
        assert_eq!(rc.select_qindex(false, 1_000_000_000), rc.qmin as u8);
    }

    #[test]
    fn keyframe_target_boosted() {
        let rc = RateControl::new(&cfg(500));
        let avg = rc.avg_frame_bits();
        let kf = rc.frame_target_bits(true) as f64;
        assert!(
            kf > avg * 5.0,
            "keyframe target {kf} not boosted over avg {avg}"
        );
    }

    #[test]
    fn balance_accounting_credits_underspend() {
        let mut rc = RateControl::new(&cfg(500));
        let target = rc.frame_target_bits(false);
        // Spend far less than target: balance goes positive, next target rises.
        rc.update_after_encode(false, 200, target, target / 4);
        let next = rc.frame_target_bits(false);
        assert!(next > target, "underspend should raise next target");
    }

    #[test]
    fn balance_accounting_debits_overspend() {
        let mut rc = RateControl::new(&cfg(500));
        let target = rc.frame_target_bits(false);
        rc.update_after_encode(false, 200, target, target * 4);
        let next = rc.frame_target_bits(false);
        assert!(next < target, "overspend should lower next target");
    }

    #[test]
    fn keyframe_does_not_move_balance() {
        let mut rc = RateControl::new(&cfg(500));
        let before = rc.bits_balance;
        let target = rc.frame_target_bits(true);
        rc.update_after_encode(true, 200, target, target * 3);
        assert_eq!(
            rc.bits_balance, before,
            "keyframe must not touch the balance"
        );
    }

    #[test]
    fn correction_factor_clamped_per_frame() {
        let mut rc = RateControl::new(&cfg(500));
        // A single wildly-overshooting frame can raise the factor by at most ×1.1.
        let cf0 = rc.inter_correction;
        rc.update_after_encode(false, 200, 1_000, 1_000_000_000);
        assert!(rc.inter_correction <= cf0 * CORRECTION_ADJ_MAX + 1e-9);
        // And a wildly-undershooting frame lowers it by at most ×0.9.
        let cf1 = rc.inter_correction;
        rc.update_after_encode(false, 200, 1_000_000_000, 1);
        assert!(rc.inter_correction >= cf1 * CORRECTION_ADJ_MIN - 1e-9);
    }

    #[test]
    fn correction_factor_stays_in_bounds() {
        let mut rc = RateControl::new(&cfg(500));
        for _ in 0..1000 {
            rc.update_after_encode(false, 240, 1_000, 1_000_000_000);
        }
        assert!(rc.inter_correction <= MAX_BPB_FACTOR + 1e-9);
        for _ in 0..1000 {
            rc.update_after_encode(false, 160, 1_000_000_000, 1);
        }
        assert!(rc.inter_correction >= MIN_BPB_FACTOR - 1e-9);
    }

    #[test]
    fn update_bitrate_raises_targets_and_damps_balance() {
        let mut rc = RateControl::new(&cfg(300));
        // Accumulate some debt, then quadruple the bitrate.
        rc.update_after_encode(false, 200, 1_000, 100_000);
        let balance_before = rc.bits_balance;
        let lo_avg = rc.avg_frame_bits();
        rc.update_bitrate_kbps(1200);
        assert!((rc.avg_frame_bits() - 4.0 * lo_avg).abs() < 1e-6);
        // Balance damped toward zero, not reset.
        assert_eq!(rc.bits_balance, balance_before * 0.5);
        assert_ne!(rc.bits_balance, 0.0);
    }

    #[test]
    fn higher_bitrate_picks_lower_or_equal_qindex() {
        let lo = RateControl::new(&cfg(300));
        let hi = RateControl::new(&cfg(1200));
        let q_lo = lo.select_qindex(false, lo.frame_target_bits(false));
        let q_hi = hi.select_qindex(false, hi.frame_target_bits(false));
        assert!(q_hi <= q_lo, "higher bitrate should not raise qindex");
    }
}
