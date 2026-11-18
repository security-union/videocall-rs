// Golden tests: every SPL function is run on a few thousand pseudo-random
// inputs (full range, extremes and small values) and all outputs are folded
// into one FNV-1a hash per group. The expected hashes come from the real
// WebRTC generic-C sources at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd,
// built by `golden/build.sh` (see `golden/golden_gen.cc`, which this file
// mirrors line by line). A mismatch means the port is not bit-exact.

use super::test_util::{Hash, Rng};
use super::*;
use crate::neteq::cross_correlation::cross_correlation_with_auto_shift;

#[test]
fn golden_inline() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..4000 {
        let x = r.i32();
        let y = r.i32();
        h.feed(norm_w32(x) as i64);
        h.feed(norm_w16(x as i16) as i64);
        h.feed(norm_u32(x as u32) as i64);
        h.feed(get_size_in_bits(x as u32) as i64);
        h.feed(add_sat_w32(x, y) as i64);
        h.feed(sub_sat_w32(x, y) as i64);
        h.feed(sat_w32_to_w16(x) as i64);
        h.feed(add_sat_w16(x as i16, y as i16) as i64);
        h.feed(sub_sat_w16(x as i16, y as i16) as i64);
        h.feed(count_leading_zeros_32(x as u32) as i64);
        h.feed(count_leading_zeros_64(((x as u32 as u64) << 32) | y as u32 as u64) as i64);
    }
    assert_eq!(0xdc4194beb6ebe200, h.value());
}

#[test]
fn golden_macros() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..4000 {
        let x = r.i32();
        let c = r.range(-31, 30);
        let a = r.i16();
        let b = r.i16();
        let s = r.range(0, 15);
        h.feed(spl_shift_w32(x, c) as i64);
        h.feed(spl_abs_w16(x as i16) as i64);
        h.feed(spl_abs_w32(x) as i64);
        h.feed(spl_mul_16_16(a, b) as i64);
        h.feed(spl_mul_16_16_rsft(a, b, s) as i64);
    }
    assert_eq!(0x0563aa3fbf0243d1, h.value());
}

#[test]
fn golden_division() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..4000 {
        let num = r.i32();
        let den = r.i16();
        if !(num == i32::MIN && den == -1) {
            h.feed(div_w32_w16(num, den) as i64);
            h.feed(div_w32_w16_res_w16(num, den) as i64);
        }
        h.feed(div_u32_u16(num as u32, den as u16) as i64);
        let num2 = (r.next() & 0x7fff_ffff) as i32;
        let hi = r.range(16384, 32767) as i16;
        let low = r.range(0, 32767) as i16;
        h.feed(div_w32_hi_low(num2, hi, low) as i64);
    }
    assert_eq!(0xcc04d9287865957e, h.value());
}

#[test]
fn golden_sqrt() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..4000 {
        let x = r.i32();
        h.feed(sqrt(x) as i64);
        h.feed(sqrt_floor(x) as i64);
        let next = r.next();
        let x2 = (next >> r.range(0, 31)) as i32;
        h.feed(sqrt(x2) as i64);
        h.feed(sqrt_floor(x2) as i64);
    }
    assert_eq!(0x31ac9f9de5d6dca3, h.value());
}

#[test]
fn golden_min_max() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    let mut v16 = [0i16; 64];
    let mut v32 = [0i32; 64];
    for _ in 0..2000 {
        let len = r.urange(1, 64);
        r.fill16(&mut v16[..len]);
        r.fill32(&mut v32[..len]);
        h.feed(max_abs_value_w16(&v16, len) as i64);
        h.feed(max_abs_value_w32(&v32, len) as i64);
        h.feed(max_abs_element_w16(&v16, len) as i64);
        h.feed(max_index_w16(&v16, len) as i64);
        h.feed(max_index_w32(&v32, len) as i64);
        h.feed(min_index_w16(&v16, len) as i64);
        h.feed(min_index_w32(&v32, len) as i64);
        h.feed(max_value_w16(&v16, len) as i64);
        h.feed(max_value_w32(&v32, len) as i64);
        h.feed(min_value_w16(&v16, len) as i64);
        h.feed(min_value_w32(&v32, len) as i64);
        h.feed(max_abs_index_w16(&v16, len) as i64);
        let (mn, mx) = min_max_w16(&v16, len);
        h.feed(mn as i64);
        h.feed(mx as i64);
    }
    assert_eq!(0x819819051171ee02, h.value());
}

/// Shared by the SPL and the NetEq cross-correlation golden tests.
pub(crate) fn golden_cross_correlation_hash(auto_shift: bool) -> u64 {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..2000 {
        let dim_seq = r.urange(1, 40);
        let dim_cc = r.urange(1, 20);
        let step = r.range(-2, 2);
        let shifts = r.range(0, 12);
        let seq1 = r.vec16(dim_seq);
        let span = step.unsigned_abs() as usize * (dim_cc - 1);
        let seq2 = r.vec16(dim_seq + span);
        let offset = if step < 0 { span } else { 0 };
        let mut out = vec![0i32; dim_cc];
        if auto_shift {
            h.feed(cross_correlation_with_auto_shift(
                &seq1, &seq2, offset, dim_seq, dim_cc, step, &mut out,
            ) as i64);
        } else {
            cross_correlation(
                &mut out, &seq1, &seq2, offset, dim_seq, dim_cc, shifts, step,
            );
        }
        h.feed32(&out);
    }
    h.value()
}

#[test]
fn golden_cross_correlation() {
    assert_eq!(0x08112778fc334e6c, golden_cross_correlation_hash(false));
}

#[test]
fn golden_downsample_fast() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..2000 {
        let coef_len = r.urange(1, 8);
        let factor = r.range(1, 12);
        let delay = r.urange(0, 4);
        let out_len = r.urange(0, 10);
        let coefs = r.vec16(coef_len);
        let offset = coef_len - 1;
        let needed = if out_len == 0 {
            0
        } else {
            delay + factor as usize * (out_len - 1) + 1
        };
        let input = r.vec16(offset + needed + 8);
        let in_len = (needed as i32 + r.range(-2, 3)).max(0) as usize;
        let mut out = vec![0i16; out_len + 1];
        let ret = downsample_fast(
            &input, offset, in_len, &mut out, out_len, &coefs, coef_len, factor, delay,
        );
        h.feed(ret as i64);
        if ret == 0 {
            h.feed16(&out[..out_len]);
        }
    }
    assert_eq!(0x25c9ef4a9d8b62eb, h.value());
}

#[test]
fn golden_dot_product_with_scale() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    let mut a = [0i16; 100];
    let mut b = [0i16; 100];
    for _ in 0..2000 {
        let len = r.urange(0, 100);
        let scaling = r.range(0, 20);
        r.fill16(&mut a[..len]);
        r.fill16(&mut b[..len]);
        h.feed(dot_product_with_scale(&a, &b, len, scaling) as i64);
    }
    assert_eq!(0x22225b3373a6f6c5, h.value());
}

#[test]
fn golden_vector_scaling() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    let mut in1 = [0i16; 32];
    let mut in2 = [0i16; 32];
    let mut out = [0i16; 32];
    let mut in32 = [0i32; 32];
    for _ in 0..2000 {
        let len = r.urange(1, 32);
        r.fill16(&mut in1[..len]);
        r.fill16(&mut in2[..len]);
        r.fill32(&mut in32[..len]);

        let rs = r.range(-3, 15) as i16;
        vector_bit_shift_w16(&mut out, len, &in1, rs);
        h.feed16(&out[..len]);
        let rs = r.range(-4, 24);
        vector_bit_shift_w32_to_w16(&mut out, len, &in32, rs);
        h.feed16(&out[..len]);
        let gain = r.i16();
        let rs = r.range(0, 15) as i16;
        scale_vector(&in1, &mut out, gain, len, rs);
        h.feed16(&out[..len]);
        scale_vector_with_sat(&in1, &mut out, gain, len, rs);
        h.feed16(&out[..len]);
        let g1 = r.i16();
        let g2 = r.i16();
        let s1 = r.range(0, 15);
        let s2 = r.range(0, 15);
        scale_and_add_vectors(&in1, g1, s1, &in2, g2, s2, &mut out, len);
        h.feed16(&out[..len]);
        let rrs = r.range(-1, 15);
        let ret = scale_and_add_vectors_with_round(&in1, g1, &in2, g2, rrs, &mut out, len);
        h.feed(ret as i64);
        if ret == 0 {
            h.feed16(&out[..len]);
        }
        let add = r.i32();
        let ars = r.range(0, 15) as i16;
        affine_transform_vector(&mut out, &in1, gain, add, ars, len);
        h.feed16(&out[..len]);
        r.fill16(&mut out[..len]);
        add_affine_vector_to_vector(&mut out, &in2, gain, add, ars, len);
        h.feed16(&out[..len]);
        let rs = r.range(0, 15) as i16;
        elementwise_vector_mult(&mut out, &in1, &in2, len, rs);
        h.feed16(&out[..len]);
    }
    assert_eq!(0x21c6b06bb4bf15ca, h.value());
}

#[test]
fn golden_filters() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..2000 {
        // FilterARFastQ12
        let coef_len = r.urange(2, 8);
        let data_len = r.urange(1, 40);
        let coefs = r.vec16(coef_len);
        let input = r.vec16(data_len);
        let mut out = r.vec16(coef_len - 1 + data_len);
        filter_ar_fast_q12(&input, &mut out, coef_len - 1, &coefs, coef_len, data_len);
        h.feed16(&out);
        // FilterMAFastQ12
        let b_len = r.urange(1, 8);
        let len = r.urange(1, 40);
        let b = r.vec16(b_len);
        let min_in = r.vec16(b_len - 1 + len);
        let mut mout = vec![0i16; len];
        filter_ma_fast_q12(&min_in, b_len - 1, &mut mout, &b, b_len, len);
        h.feed16(&mout);
        // FilterAR
        let a_len = r.urange(1, 8);
        let state_len = r.range((a_len as i32 - 1).max(1), a_len as i32 + 3) as usize;
        let x_len = r.range((a_len as i32 - 1).max(1), 30) as usize;
        let a = r.vec16(a_len);
        let x = r.vec16(x_len);
        let mut st = r.vec16(state_len);
        let mut stl = r.vec16(state_len);
        let mut f = vec![0i16; x_len];
        let mut fl = vec![0i16; x_len];
        h.feed(filter_ar(
            &a, a_len, &x, x_len, &mut st, state_len, &mut stl, &mut f, &mut fl,
        ) as i64);
        h.feed16(&f);
        h.feed16(&fl);
        h.feed16(&st);
        h.feed16(&stl);
    }
    assert_eq!(0x653da79b900eb78b, h.value());
}

#[test]
fn golden_lpc() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..2000 {
        let len = r.urange(1, 80);
        let amp_shift = r.range(0, 12);
        let v: Vec<i16> = (0..len).map(|_| r.i16() >> amp_shift).collect();
        let order = r.urange(0, len.min(20));
        let mut ac = [0i32; 21];
        let mut scale = 0;
        h.feed(auto_correlation(&v, len, order, &mut ac, &mut scale) as i64);
        h.feed(scale as i64);
        h.feed32(&ac[..order + 1]);

        let lorder = r.urange(1, 14);
        let mut big_r = [0i32; 15];
        if r.below(2) == 0 {
            let l2 = r.urange(lorder + 1, 80);
            let w: Vec<i16> = (0..l2).map(|_| r.i16() >> amp_shift).collect();
            auto_correlation(&w, l2, lorder, &mut big_r, &mut scale);
        } else {
            big_r[0] = (r.next() & 0x7fff_ffff) as i32;
            for value in big_r.iter_mut().take(lorder + 1).skip(1) {
                *value = r.i32();
            }
        }
        let mut a = [0i16; 15];
        let mut k = [0i16; 15];
        h.feed(levinson_durbin(&big_r, &mut a, &mut k, lorder) as i64);
        h.feed16(&a);
        h.feed16(&k);

        let mut en_scale = 0;
        h.feed(energy(&v, len, &mut en_scale) as i64);
        h.feed(en_scale as i64);
        let times = r.urange(1, 1000);
        h.feed(get_scaling_square(&v, len, times) as i64);

        let hsize = r.urange(1, 600);
        let mut win = vec![0i16; hsize];
        get_hanning_window(&mut win, hsize);
        h.feed16(&win);
    }
    assert_eq!(0x60ca5d2da285ce42, h.value());
}

#[test]
fn golden_rand() {
    let mut r = Rng::new();
    let mut h = Hash::new();
    for _ in 0..2000 {
        let mut seed = r.next();
        for _ in 0..8 {
            h.feed(rand_u(&mut seed) as i64);
            h.feed(rand_n(&mut seed) as i64);
        }
        let mut v = [0i16; 5];
        h.feed(rand_u_array(&mut v, 5, &mut seed) as i64);
        h.feed16(&v);
        h.feed(seed as i64);
    }
    assert_eq!(0x479179592bab6540, h.value());
}
