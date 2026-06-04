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

//! Pure-Rust bilinear downscaler for I420 frames.
//!
//! Used by the costume video path to resize the fixed 1280x720 sprite-sheet
//! frames to whatever resolution the AQ controller requests.

/// Downscale a source I420 frame to the target dimensions using bilinear
/// interpolation. Each plane (Y at full resolution, U/V at half resolution)
/// is scaled independently.
///
/// # Panics
///
/// Panics (via debug_assert) if `dst` is smaller than
/// `dst_w * dst_h * 3 / 2` bytes, or if `src` is smaller than
/// `src_w * src_h * 3 / 2` bytes.
pub fn scale_i420(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;

    debug_assert!(src.len() >= src_w * src_h * 3 / 2);
    debug_assert!(dst.len() >= dst_w * dst_h * 3 / 2);

    // I420 plane layout — source
    let src_y = &src[..src_w * src_h];
    let src_u_offset = src_w * src_h;
    let src_u = &src[src_u_offset..src_u_offset + (src_w / 2) * (src_h / 2)];
    let src_v_offset = src_u_offset + (src_w / 2) * (src_h / 2);
    let src_v = &src[src_v_offset..src_v_offset + (src_w / 2) * (src_h / 2)];

    // I420 plane layout — destination
    let dst_y_len = dst_w * dst_h;
    let dst_uv_len = (dst_w / 2) * (dst_h / 2);
    let (dst_y, dst_uv) = dst[..dst_y_len + 2 * dst_uv_len].split_at_mut(dst_y_len);
    let (dst_u, dst_v) = dst_uv.split_at_mut(dst_uv_len);

    // Scale Y plane (full resolution)
    scale_plane_bilinear(src_y, src_w, src_h, dst_y, dst_w, dst_h);

    // Scale U plane (half resolution in both dimensions)
    scale_plane_bilinear(src_u, src_w / 2, src_h / 2, dst_u, dst_w / 2, dst_h / 2);

    // Scale V plane (half resolution in both dimensions)
    scale_plane_bilinear(src_v, src_w / 2, src_h / 2, dst_v, dst_w / 2, dst_h / 2);
}

/// Bilinear interpolation scaler for a single 8-bit plane.
///
/// Maps each destination pixel to a floating-point position in the source,
/// then blends the 4 nearest source pixels proportionally.
#[inline(never)] // keep out of hot loop inlining budget — called 3x per frame
fn scale_plane_bilinear(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
) {
    // Pre-compute scaling ratios. We map the center of each destination
    // pixel to the corresponding source position.
    let x_ratio = src_w as f32 / dst_w as f32;
    let y_ratio = src_h as f32 / dst_h as f32;

    let src_w_max = (src_w - 1) as f32;
    let src_h_max = (src_h - 1) as f32;

    for dy in 0..dst_h {
        // Source Y coordinate (center-mapped)
        let sy_f = ((dy as f32 + 0.5) * y_ratio - 0.5).clamp(0.0, src_h_max);
        let sy_floor = sy_f as usize;
        let sy_ceil = (sy_floor + 1).min(src_h - 1);
        let y_frac = sy_f - sy_floor as f32;
        let y_frac_inv = 1.0 - y_frac;

        let src_row_top = sy_floor * src_w;
        let src_row_bot = sy_ceil * src_w;
        let dst_row = dy * dst_w;

        for dx in 0..dst_w {
            // Source X coordinate (center-mapped)
            let sx_f = ((dx as f32 + 0.5) * x_ratio - 0.5).clamp(0.0, src_w_max);
            let sx_floor = sx_f as usize;
            let sx_ceil = (sx_floor + 1).min(src_w - 1);
            let x_frac = sx_f - sx_floor as f32;
            let x_frac_inv = 1.0 - x_frac;

            // Fetch 4 neighbors
            let tl = src[src_row_top + sx_floor] as f32;
            let tr = src[src_row_top + sx_ceil] as f32;
            let bl = src[src_row_bot + sx_floor] as f32;
            let br = src[src_row_bot + sx_ceil] as f32;

            // Bilinear blend
            let top = tl * x_frac_inv + tr * x_frac;
            let bot = bl * x_frac_inv + br * x_frac;
            let val = top * y_frac_inv + bot * y_frac;

            dst[dst_row + dx] = val.round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a solid-color I420 frame.
    fn solid_i420(w: u32, h: u32, y_val: u8, u_val: u8, v_val: u8) -> Vec<u8> {
        let w = w as usize;
        let h = h as usize;
        let mut buf = vec![0u8; w * h * 3 / 2];
        // Y plane
        for p in buf[..w * h].iter_mut() {
            *p = y_val;
        }
        // U plane
        let u_start = w * h;
        let u_end = u_start + (w / 2) * (h / 2);
        for p in buf[u_start..u_end].iter_mut() {
            *p = u_val;
        }
        // V plane
        let v_end = u_end + (w / 2) * (h / 2);
        for p in buf[u_end..v_end].iter_mut() {
            *p = v_val;
        }
        buf
    }

    #[test]
    fn solid_frame_stays_solid_after_scale() {
        let src = solid_i420(1280, 720, 200, 100, 150);
        let mut dst = vec![0u8; 640 * 360 * 3 / 2];
        scale_i420(&src, 1280, 720, &mut dst, 640, 360);

        // All Y pixels should be 200
        for &p in &dst[..640 * 360] {
            assert_eq!(p, 200, "Y plane mismatch");
        }
        // All U pixels should be 100
        let u_start = 640 * 360;
        let u_end = u_start + 320 * 180;
        for &p in &dst[u_start..u_end] {
            assert_eq!(p, 100, "U plane mismatch");
        }
        // All V pixels should be 150
        let v_end = u_end + 320 * 180;
        for &p in &dst[u_end..v_end] {
            assert_eq!(p, 150, "V plane mismatch");
        }
    }

    #[test]
    fn identity_scale_preserves_frame() {
        // 4x4 frame with a simple gradient in Y
        let w: u32 = 4;
        let h: u32 = 4;
        let mut src = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                src[y * w as usize + x] = (y * w as usize + x) as u8 * 10;
            }
        }
        // U/V constant
        let uv_start = (w * h) as usize;
        for p in src[uv_start..].iter_mut() {
            *p = 128;
        }

        let mut dst = vec![0u8; (w * h * 3 / 2) as usize];
        scale_i420(&src, w, h, &mut dst, w, h);

        // Identity scale should produce the same frame (within rounding)
        for i in 0..src.len() {
            let diff = (src[i] as i16 - dst[i] as i16).unsigned_abs();
            assert!(
                diff <= 1,
                "pixel {} diverged: src={} dst={}",
                i,
                src[i],
                dst[i]
            );
        }
    }

    #[test]
    fn downscale_1280x720_to_960x540() {
        // Just verify it doesn't panic and produces the right buffer length
        let src = solid_i420(1280, 720, 128, 128, 128);
        let mut dst = vec![0u8; 960 * 540 * 3 / 2];
        scale_i420(&src, 1280, 720, &mut dst, 960, 540);
        // Should all be 128
        assert!(dst.iter().all(|&p| p == 128));
    }

    #[test]
    fn downscale_1280x720_to_854x480() {
        let src = solid_i420(1280, 720, 64, 200, 32);
        // 854 is not evenly divisible by 2 — use 854 which gives chroma 427.
        // Actually for I420, width must be even. AQ tiers use 854 which is even.
        let dst_w: u32 = 854;
        let dst_h: u32 = 480;
        let mut dst = vec![0u8; (dst_w * dst_h * 3 / 2) as usize];
        scale_i420(&src, 1280, 720, &mut dst, dst_w, dst_h);

        // Y should be 64
        for &p in &dst[..(dst_w * dst_h) as usize] {
            assert_eq!(p, 64);
        }
    }
}
