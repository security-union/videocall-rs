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

//! Peak signal-to-noise ratio (PSNR) metrics for 8-bit planes and I420 frames.

/// PSNR in decibels between two equal-length 8-bit planes.
///
/// Returns [`f64::INFINITY`] when the planes are identical (zero error).
///
/// # Panics
/// Panics if the slices differ in length.
pub fn psnr_plane(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "PSNR planes must have equal length");
    if a.is_empty() {
        return f64::INFINITY;
    }
    let mut sum_sq = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let d = x as f64 - y as f64;
        sum_sq += d * d;
    }
    if sum_sq == 0.0 {
        return f64::INFINITY;
    }
    let mse = sum_sq / a.len() as f64;
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// Per-plane PSNR (in decibels) for an I420 frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YuvPsnr {
    /// Luma plane PSNR.
    pub y: f64,
    /// Cb plane PSNR.
    pub u: f64,
    /// Cr plane PSNR.
    pub v: f64,
}

/// PSNR between two I420 frames of the given dimensions, computed per plane.
///
/// Chroma planes use ceiling division to match [`super::i420::buffer_size_i420`].
///
/// # Panics
/// Panics if either buffer is shorter than the expected I420 size.
pub fn psnr_i420(a: &[u8], b: &[u8], width: u32, height: u32) -> YuvPsnr {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let y_size = w * h;
    let c_size = cw * ch;

    let ay = &a[0..y_size];
    let au = &a[y_size..y_size + c_size];
    let av = &a[y_size + c_size..y_size + 2 * c_size];
    let by = &b[0..y_size];
    let bu = &b[y_size..y_size + c_size];
    let bv = &b[y_size + c_size..y_size + 2 * c_size];

    YuvPsnr {
        y: psnr_plane(ay, by),
        u: psnr_plane(au, bu),
        v: psnr_plane(av, bv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_planes_are_infinite() {
        let a = vec![7u8; 100];
        assert_eq!(psnr_plane(&a, &a), f64::INFINITY);
    }

    #[test]
    fn known_error_matches_formula() {
        // Every sample differs by 1 → MSE = 1 → PSNR = 20*log10(255) ≈ 48.13 dB.
        let a = vec![100u8; 16];
        let b = vec![101u8; 16];
        let p = psnr_plane(&a, &b);
        assert!((p - 48.13).abs() < 0.1, "got {p}");
    }

    #[test]
    fn i420_splits_planes() {
        let size = super::super::i420::buffer_size_i420(64, 64);
        let a = vec![50u8; size];
        let b = vec![50u8; size];
        let p = psnr_i420(&a, &b, 64, 64);
        assert_eq!(p.y, f64::INFINITY);
        assert_eq!(p.u, f64::INFINITY);
        assert_eq!(p.v, f64::INFINITY);
    }
}
