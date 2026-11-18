// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/levinson_durbin.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_LevinsonDurbin`.
//!
//! The C function is compiled with `RTC_NO_SANITIZE("signed-integer-overflow")`
//! (bugs.webrtc.org/5486): it relies on `int32_t` arithmetic wrapping. Every
//! such operation is `wrapping_*` here. `int16_t * int16_t` products are
//! `int` and cannot overflow on their own; `(int16_t)` casts truncate and
//! `>>` on negatives is arithmetic, as on every compiler WebRTC supports.

use super::division_operations::div_w32_hi_low;
use super::signal_processing_library::{spl_abs_w16, spl_abs_w32};
use super::spl_inl::norm_w32;

const SPL_LEVINSON_MAXORDER: usize = 20;

/// Splits a Q31 value into the C "hi and low format".
#[inline]
fn hi_low(x: i32) -> (i16, i16) {
    let hi = (x >> 16) as i16;
    let low = (x.wrapping_sub((hi as i32).wrapping_mul(65536)) >> 1) as i16;
    (hi, low)
}

#[inline]
fn mul(a: i16, b: i16) -> i32 {
    a as i32 * b as i32
}

/// C `WebRtcSpl_LevinsonDurbin`. Returns 1 for a stable filter, 0 otherwise.
/// `auto_corr` needs `order + 1` values (at least 2), `lpc_coef` `order + 1`
/// and `refl_coef` `order`.
pub fn levinson_durbin(
    auto_corr: &[i32],
    lpc_coef: &mut [i16],
    refl_coef: &mut [i16],
    order: usize,
) -> i16 {
    let r = auto_corr;
    let a = lpc_coef;
    let k = refl_coef;

    let mut r_hi = [0i16; SPL_LEVINSON_MAXORDER + 1];
    let mut r_low = [0i16; SPL_LEVINSON_MAXORDER + 1];
    let mut a_hi = [0i16; SPL_LEVINSON_MAXORDER + 1];
    let mut a_low = [0i16; SPL_LEVINSON_MAXORDER + 1];
    let mut a_upd_hi = [0i16; SPL_LEVINSON_MAXORDER + 1];
    let mut a_upd_low = [0i16; SPL_LEVINSON_MAXORDER + 1];

    // Normalize the autocorrelation R[0]...R[order+1].
    let mut norm = norm_w32(r[0]);
    for i in 0..=order {
        let temp1 = r[i].wrapping_shl(norm as u32);
        (r_hi[i], r_low[i]) = hi_low(temp1);
    }

    // K = A[1] = -R[1] / R[0]
    let temp2 = r[1].wrapping_shl(norm as u32); // R[1] in Q31
    let temp3 = spl_abs_w32(temp2); // abs R[1]
    let mut temp1 = div_w32_hi_low(temp3, r_hi[0], r_low[0]); // abs(R[1])/R[0] in Q31
    if temp2 > 0 {
        temp1 = temp1.wrapping_neg();
    }

    let (mut k_hi, mut k_low) = hi_low(temp1);
    k[0] = k_hi;

    temp1 >>= 4; // A[1] in Q27.
    (a_hi[1], a_low[1]) = hi_low(temp1);

    // Alpha = R[0] * (1-K^2)
    temp1 = ((mul(k_hi, k_low) >> 14).wrapping_add(mul(k_hi, k_hi))).wrapping_mul(2); // k^2 in Q31
    temp1 = spl_abs_w32(temp1); // Guard against <0
    temp1 = 0x7fff_ffffi32.wrapping_sub(temp1); // (1 - K[0]*K[0]) in Q31

    let tmp_hi = (temp1 >> 16) as i16;
    let tmp_low = (temp1.wrapping_sub((tmp_hi as i32) << 16) >> 1) as i16;

    // Alpha in Q31.
    temp1 = (mul(r_hi[0], tmp_hi)
        .wrapping_add(mul(r_hi[0], tmp_low) >> 15)
        .wrapping_add(mul(r_low[0], tmp_hi) >> 15))
    .wrapping_shl(1);

    // Normalize Alpha and put it in hi and low format.
    let mut alpha_exp: i16 = norm_w32(temp1);
    temp1 = temp1.wrapping_shl(alpha_exp as u32);
    let mut alpha_hi = (temp1 >> 16) as i16;
    let mut alpha_low = (temp1.wrapping_sub((alpha_hi as i32) << 16) >> 1) as i16;

    for i in 2..=order {
        //   temp1W32 = R[i] + sum_{j=1..i-1} R[j]*A[i-j]
        temp1 = 0;
        for j in 1..i {
            temp1 = temp1
                .wrapping_add(mul(r_hi[j], a_hi[i - j]).wrapping_mul(2))
                .wrapping_add(
                    ((mul(r_hi[j], a_low[i - j]) >> 15)
                        .wrapping_add(mul(r_low[j], a_hi[i - j]) >> 15))
                    .wrapping_mul(2),
                );
        }

        temp1 = temp1.wrapping_mul(16);
        temp1 = temp1
            .wrapping_add((r_hi[i] as i32).wrapping_mul(65536))
            .wrapping_add((r_low[i] as i32).wrapping_shl(1));

        // K = -temp1W32 / Alpha
        let temp2 = spl_abs_w32(temp1);
        let mut temp3 = div_w32_hi_low(temp2, alpha_hi, alpha_low);

        if temp1 > 0 {
            temp3 = temp3.wrapping_neg();
        }

        // Use the Alpha shifts from earlier to de-normalize.
        norm = norm_w32(temp3);
        if alpha_exp <= norm || temp3 == 0 {
            temp3 = temp3.wrapping_shl(alpha_exp as u32);
        } else if temp3 > 0 {
            temp3 = 0x7fff_ffff;
        } else {
            temp3 = i32::MIN;
        }

        (k_hi, k_low) = hi_low(temp3);

        // Store Reflection coefficient in Q15.
        k[i - 1] = k_hi;

        // Test for unstable filter.
        if spl_abs_w16(k_hi) > 32750 {
            return 0;
        }

        // Anew[j] = A[j] + K*A[i-j] for j=1..i-1; Anew[i] = K
        for j in 1..i {
            temp1 = (a_hi[j] as i32)
                .wrapping_mul(65536)
                .wrapping_add((a_low[j] as i32).wrapping_shl(1));
            temp1 = temp1.wrapping_add(
                (mul(k_hi, a_hi[i - j])
                    .wrapping_add(mul(k_hi, a_low[i - j]) >> 15)
                    .wrapping_add(mul(k_low, a_hi[i - j]) >> 15))
                .wrapping_mul(2),
            );
            (a_upd_hi[j], a_upd_low[j]) = hi_low(temp1);
        }

        // temp3W32 = K in Q27 (Convert from Q31 to Q27)
        temp3 >>= 4;
        (a_upd_hi[i], a_upd_low[i]) = hi_low(temp3);

        // Alpha = Alpha * (1-K^2)
        temp1 = ((mul(k_hi, k_low) >> 14).wrapping_add(mul(k_hi, k_hi))).wrapping_mul(2);
        temp1 = spl_abs_w32(temp1);
        temp1 = 0x7fff_ffffi32.wrapping_sub(temp1);

        let tmp_hi = (temp1 >> 16) as i16;
        let tmp_low = (temp1.wrapping_sub((tmp_hi as i32) << 16) >> 1) as i16;

        temp1 = (mul(alpha_hi, tmp_hi)
            .wrapping_add(mul(alpha_hi, tmp_low) >> 15)
            .wrapping_add(mul(alpha_low, tmp_hi) >> 15))
        .wrapping_shl(1);

        norm = norm_w32(temp1);
        temp1 = temp1.wrapping_shl(norm as u32);

        alpha_hi = (temp1 >> 16) as i16;
        alpha_low = (temp1.wrapping_sub((alpha_hi as i32) << 16) >> 1) as i16;

        // `Alpha_exp + norm` is an `int` stored back into `int16_t`.
        alpha_exp = (alpha_exp as i32 + norm as i32) as i16;

        a_hi[1..=i].copy_from_slice(&a_upd_hi[1..=i]);
        a_low[1..=i].copy_from_slice(&a_upd_low[1..=i]);
    }

    // Set A[0] to 1.0 and store A[i] i=1...order in Q12 (from Q27, rounded).
    a[0] = 4096;
    for i in 1..=order {
        temp1 = (a_hi[i] as i32)
            .wrapping_mul(65536)
            .wrapping_add((a_low[i] as i32).wrapping_shl(1));
        a[i] = (temp1.wrapping_mul(2).wrapping_add(32768) >> 16) as i16;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.EstimatorsTest
    #[test]
    fn estimators_test() {
        const ORDER: usize = 2;
        let unstable_filter: [i32; 4] = [4, 12, 133, 1100];
        let stable_filter: [i32; 4] = [1100, 133, 12, 4];
        let mut lpc = [0i16; ORDER + 2];
        let mut refl = [0i16; ORDER + 2];
        assert_eq!(
            0,
            levinson_durbin(&unstable_filter, &mut lpc, &mut refl, ORDER)
        );
        assert_eq!(
            1,
            levinson_durbin(&stable_filter, &mut lpc, &mut refl, ORDER)
        );
        assert_eq!([4096, -497, 15, 0], lpc);
        assert_eq!([-3962, 123, 0, 0], refl);
    }
}
