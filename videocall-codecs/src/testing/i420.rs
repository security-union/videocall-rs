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

//! Deterministic synthetic I420 frame sources for the encoder harness.
//!
//! Each generator returns a planar I420 buffer of length [`buffer_size_i420`]:
//! a full-resolution Y plane followed by half-resolution U and V planes. All
//! sources are pure functions of their arguments (no RNG state, no `rand`
//! dependency) so tests are fully reproducible.

/// Size in bytes of a planar I420 buffer for the given dimensions.
///
/// Chroma planes use ceiling division so odd dimensions are handled without
/// panicking; for even dimensions this equals `width * height * 3 / 2`.
pub fn buffer_size_i420(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    w * h + 2 * cw * ch
}

/// Split a mutable I420 buffer into its `(y, u, v)` planes.
fn planes_mut(buf: &mut [u8], width: u32, height: u32) -> (&mut [u8], &mut [u8], &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let (y, rest) = buf.split_at_mut(w * h);
    let (u, v) = rest.split_at_mut(cw * ch);
    (y, u, v)
}

/// A smooth diagonal gradient that scrolls with `t`.
///
/// Useful as a single keyframe source: it is easy to compress yet has enough
/// structure to exercise transforms and produce a meaningful PSNR.
pub fn gradient(width: u32, height: u32, t: u32) -> Vec<u8> {
    let mut buf = vec![0u8; buffer_size_i420(width, height)];
    let (w, h) = (width as usize, height as usize);
    let cw = w.div_ceil(2);
    let (y, u, v) = planes_mut(&mut buf, width, height);
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = (col + row + t as usize) as u8;
        }
    }
    let ch = h.div_ceil(2);
    for row in 0..ch {
        for col in 0..cw {
            u[row * cw + col] = (col * 2 + t as usize) as u8;
            v[row * cw + col] = (row * 2 + t as usize) as u8;
        }
    }
    buf
}

/// A 64x64 white box on a mid-gray background, translating diagonally and
/// wrapping around the frame with `t`.
///
/// This has moving high-contrast edges, which exercises motion estimation and
/// inter prediction and yields a non-trivial (but still compressible) bitrate.
pub fn moving_box(width: u32, height: u32, t: u32) -> Vec<u8> {
    const BOX: usize = 64;
    const BG: u8 = 128;
    const FG: u8 = 255;

    let mut buf = vec![0u8; buffer_size_i420(width, height)];
    let (w, h) = (width as usize, height as usize);
    let (y, u, v) = planes_mut(&mut buf, width, height);

    // Y: gray background with a wrapping white box.
    let x0 = (t as usize * 2) % w;
    let y0 = (t as usize * 2) % h;
    for row in 0..h {
        for col in 0..w {
            let in_x = ((col + w - x0) % w) < BOX;
            let in_y = ((row + h - y0) % h) < BOX;
            y[row * w + col] = if in_x && in_y { FG } else { BG };
        }
    }
    // Chroma: neutral gray everywhere.
    for c in u.iter_mut() {
        *c = 128;
    }
    for c in v.iter_mut() {
        *c = 128;
    }
    buf
}

/// Pseudo-random noise from a hand-rolled xorshift64 generator seeded by
/// `(t, seed)`. Deterministic and independent of `rand`.
///
/// Noise is essentially incompressible, so this stresses the tokenizer and
/// rate control at the high-bitrate extreme.
pub fn noise(width: u32, height: u32, t: u32, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; buffer_size_i420(width, height)];
    // Fold t and seed into a non-zero state.
    let mut state = seed ^ (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491_4F6C_DD1D;
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_F00D;
    }
    for byte in buf.iter_mut() {
        *byte = (xorshift64(&mut state) >> 33) as u8;
    }
    buf
}

/// A rate-control test source: a smooth static gradient with moderate per-frame
/// texture that survives quantization in a graded way.
///
/// Unlike [`moving_box`] — whose sharp, high-contrast edges cost a fixed number
/// of bits at *any* quantizer, leaving the bitrate quantizer-insensitive — the
/// texture here is mid-amplitude and broadband, so raising the quantizer zeroes
/// progressively more of it and the coded size responds smoothly to `qindex`.
/// That quantizer-sensitivity is what lets the rate controller actually steer the
/// bitrate, so this is the source the M3/M4 rate-control milestones use.
///
/// The texture is a deterministic function of `(row, col, t)` (own xorshift64,
/// no `rand`), and it changes every frame, so motion compensation cannot predict
/// it away — every frame carries genuine, quantizer-gradable residual.
pub fn busy(width: u32, height: u32, t: u32) -> Vec<u8> {
    // Amplitude chosen so that, within the app's default q window (qindex
    // 160..240) at CIF-class resolutions, the coded bitrate spans well below to
    // well above typical targets — giving the rate controller room to converge.
    const TEXTURE_AMPLITUDE: i32 = 18;

    let mut buf = vec![0u8; buffer_size_i420(width, height)];
    let (w, h) = (width as usize, height as usize);
    let (y, u, v) = planes_mut(&mut buf, width, height);

    for row in 0..h {
        for col in 0..w {
            // Gentle low-frequency gradient background (compressible; predicted
            // near-perfectly by ZEROMV once coded).
            let bg = 64 + ((row as i32 + col as i32) / 8).min(127);
            // Mid-amplitude per-frame texture (the quantizer-sensitive residual).
            let mut state = (row as u64).wrapping_mul(2_654_435_761)
                ^ (col as u64).wrapping_mul(40_503)
                ^ (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            if state == 0 {
                state = 0x1234_5678_9ABC_DEF0;
            }
            let noise = ((xorshift64(&mut state) >> 33) as i32 & 0xff) - 128;
            let val = bg + noise * TEXTURE_AMPLITUDE / 128;
            y[row * w + col] = val.clamp(0, 255) as u8;
        }
    }
    // Neutral chroma keeps the source's cost in luma where the texture lives.
    for c in u.iter_mut().chain(v.iter_mut()) {
        *c = 128;
    }
    buf
}

/// Advance an xorshift64 state and return the new value. State must be non-zero.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_produce_correct_length() {
        for &(w, h) in &[(640u32, 480u32), (64, 64), (636, 476)] {
            let expected = buffer_size_i420(w, h);
            assert_eq!(gradient(w, h, 0).len(), expected);
            assert_eq!(moving_box(w, h, 5).len(), expected);
            assert_eq!(noise(w, h, 5, 42).len(), expected);
        }
    }

    #[test]
    fn buffer_size_even_matches_classic_formula() {
        assert_eq!(buffer_size_i420(640, 480), 640 * 480 * 3 / 2);
    }

    #[test]
    fn generators_are_deterministic() {
        assert_eq!(gradient(64, 64, 3), gradient(64, 64, 3));
        assert_eq!(moving_box(64, 64, 3), moving_box(64, 64, 3));
        assert_eq!(noise(64, 64, 3, 7), noise(64, 64, 3, 7));
        assert_ne!(noise(64, 64, 3, 7), noise(64, 64, 3, 8));
    }

    #[test]
    fn moving_box_moves() {
        assert_ne!(moving_box(128, 128, 0), moving_box(128, 128, 10));
    }

    #[test]
    fn busy_is_deterministic_and_changes_each_frame() {
        assert_eq!(busy(320, 240, 4), busy(320, 240, 4));
        // The per-frame texture must differ frame-to-frame so inter frames carry
        // real residual (otherwise the rate controller has nothing to steer).
        assert_ne!(busy(320, 240, 4), busy(320, 240, 5));
    }

    #[test]
    fn busy_correct_length_and_neutral_chroma() {
        for &(w, h) in &[(320u32, 240u32), (636, 476)] {
            let b = busy(w, h, 3);
            assert_eq!(b.len(), buffer_size_i420(w, h));
            // Chroma planes are neutral gray.
            assert!(b[(w * h) as usize..].iter().all(|&c| c == 128));
        }
    }
}
