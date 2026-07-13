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

//! Planar I420 frame buffer with borders and mi-aligned geometry.
//!
//! Holds the three planes (Y, U, V) each surrounded by a [`BORDER`]-pixel
//! margin, allocated to superblock-aligned strides. The *active* dimensions are
//! rounded up to whole 8x8 mode-info units — the VP9 decoder reconstructs whole
//! mi blocks, so the encoder must operate on the same mi-aligned region, with
//! the source's cropped edge replicated to fill it.
//!
//! [`FrameBuffer::extend_borders`] replicates the outermost active samples into
//! the margins, matching libvpx `extend_plane` (`vpx_scale/generic/yv12extend.c`)
//! so reference frames are ready for motion compensation in later milestones.

/// Border width in samples on every side of every plane.
pub const BORDER: usize = 64;

#[inline]
fn align_up(v: u32, a: u32) -> u32 {
    (v + a - 1) & !(a - 1)
}

/// One plane of a bordered frame.
struct Plane {
    data: Vec<u8>,
    stride: usize,
    /// Offset of the top-left active sample.
    origin: usize,
    /// Active (mi-aligned) dimensions.
    width: usize,
    height: usize,
}

impl Plane {
    fn new(active_w: usize, active_h: usize, alloc_w: usize, alloc_h: usize) -> Self {
        let stride = alloc_w + 2 * BORDER;
        let rows = alloc_h + 2 * BORDER;
        Self {
            data: vec![0u8; stride * rows],
            stride,
            origin: BORDER * stride + BORDER,
            width: active_w,
            height: active_h,
        }
    }

    #[inline]
    fn extend_borders(&mut self) {
        let (w, h, stride, origin) = (self.width, self.height, self.stride, self.origin);
        // Left/right columns.
        for r in 0..h {
            let row = origin + r * stride;
            let left = self.data[row];
            let right = self.data[row + w - 1];
            for c in 1..=BORDER {
                self.data[row - c] = left;
                self.data[row + w - 1 + c] = right;
            }
        }
        // Top/bottom rows (including the just-filled left/right margins).
        let line = w + 2 * BORDER;
        let top = origin - BORDER;
        let bottom = origin + (h - 1) * stride - BORDER;
        for k in 1..=BORDER {
            self.data.copy_within(top..top + line, top - k * stride);
            self.data
                .copy_within(bottom..bottom + line, bottom + k * stride);
        }
    }
}

/// A bordered planar I420 (8-bit 4:2:0) frame.
pub struct FrameBuffer {
    /// True (cropped) luma dimensions supplied at import.
    pub crop_width: u32,
    pub crop_height: u32,
    y: Plane,
    u: Plane,
    v: Plane,
}

impl FrameBuffer {
    /// Allocate a buffer sized for a `crop_width` x `crop_height` I420 frame.
    /// Active dimensions round up to 8 (mi units); the allocation rounds up to
    /// 64 (superblocks).
    pub fn new(crop_width: u32, crop_height: u32) -> Self {
        let y_active_w = align_up(crop_width, 8) as usize;
        let y_active_h = align_up(crop_height, 8) as usize;
        let y_alloc_w = align_up(crop_width, 64) as usize;
        let y_alloc_h = align_up(crop_height, 64) as usize;

        // Chroma is subsampled by 2 in each dimension.
        let uv_crop_w = crop_width.div_ceil(2);
        let uv_crop_h = crop_height.div_ceil(2);
        let uv_active_w = align_up(uv_crop_w, 4) as usize;
        let uv_active_h = align_up(uv_crop_h, 4) as usize;
        let uv_alloc_w = y_alloc_w / 2;
        let uv_alloc_h = y_alloc_h / 2;

        Self {
            crop_width,
            crop_height,
            y: Plane::new(y_active_w, y_active_h, y_alloc_w, y_alloc_h),
            u: Plane::new(uv_active_w, uv_active_h, uv_alloc_w, uv_alloc_h),
            v: Plane::new(uv_active_w, uv_active_h, uv_alloc_w, uv_alloc_h),
        }
    }

    /// Luma plane: `(data, origin_offset, stride, active_width, active_height)`.
    pub fn y(&self) -> (&[u8], usize, usize, usize, usize) {
        (
            &self.y.data,
            self.y.origin,
            self.y.stride,
            self.y.width,
            self.y.height,
        )
    }
    /// Mutable luma plane accessor; see [`FrameBuffer::y`].
    pub fn y_mut(&mut self) -> (&mut [u8], usize, usize) {
        (&mut self.y.data, self.y.origin, self.y.stride)
    }
    /// Chroma-U plane accessor; see [`FrameBuffer::y`].
    pub fn u(&self) -> (&[u8], usize, usize, usize, usize) {
        (
            &self.u.data,
            self.u.origin,
            self.u.stride,
            self.u.width,
            self.u.height,
        )
    }
    /// Mutable chroma-U plane accessor.
    pub fn u_mut(&mut self) -> (&mut [u8], usize, usize) {
        (&mut self.u.data, self.u.origin, self.u.stride)
    }
    /// Chroma-V plane accessor; see [`FrameBuffer::y`].
    pub fn v(&self) -> (&[u8], usize, usize, usize, usize) {
        (
            &self.v.data,
            self.v.origin,
            self.v.stride,
            self.v.width,
            self.v.height,
        )
    }
    /// Mutable chroma-V plane accessor.
    pub fn v_mut(&mut self) -> (&mut [u8], usize, usize) {
        (&mut self.v.data, self.v.origin, self.v.stride)
    }

    /// Number of 8x8 mode-info columns/rows for the luma plane.
    pub fn mi_cols(&self) -> u32 {
        (self.crop_width + 7) >> 3
    }
    pub fn mi_rows(&self) -> u32 {
        (self.crop_height + 7) >> 3
    }

    /// Import a tight-packed I420 buffer (`w` x `h` Y, then `⌈w/2⌉ x ⌈h/2⌉` U and
    /// V). The cropped edge is replicated to fill each plane's mi-aligned active
    /// region. Returns an error on a size mismatch or short input.
    pub fn import_i420(&mut self, src: &[u8], w: u32, h: u32) -> Result<(), String> {
        if w != self.crop_width || h != self.crop_height {
            return Err(format!(
                "import size {w}x{h} != buffer {}x{}",
                self.crop_width, self.crop_height
            ));
        }
        let (yw, yh) = (w as usize, h as usize);
        let (cw, ch) = ((w.div_ceil(2)) as usize, (h.div_ceil(2)) as usize);
        let expected = yw * yh + 2 * cw * ch;
        if src.len() < expected {
            return Err(format!("i420 input too short: {} < {expected}", src.len()));
        }
        let (y_src, rest) = src.split_at(yw * yh);
        let (u_src, v_src) = rest.split_at(cw * ch);
        import_plane(&mut self.y, y_src, yw, yh);
        import_plane(&mut self.u, u_src, cw, ch);
        import_plane(&mut self.v, v_src, cw, ch);
        Ok(())
    }

    /// Replicate the active edges into every plane's border.
    pub fn extend_borders(&mut self) {
        self.y.extend_borders();
        self.u.extend_borders();
        self.v.extend_borders();
    }

    /// Export the active region back to a tight-packed I420 buffer at the
    /// cropped dimensions.
    pub fn export_i420(&self) -> Vec<u8> {
        let (yw, yh) = (self.crop_width as usize, self.crop_height as usize);
        let (cw, ch) = (
            (self.crop_width.div_ceil(2)) as usize,
            (self.crop_height.div_ceil(2)) as usize,
        );
        let mut out = Vec::with_capacity(yw * yh + 2 * cw * ch);
        export_plane(&self.y, yw, yh, &mut out);
        export_plane(&self.u, cw, ch, &mut out);
        export_plane(&self.v, cw, ch, &mut out);
        out
    }
}

/// Copy `crop_w` x `crop_h` samples into a plane's active region, replicating
/// the last column/row out to the mi-aligned active width/height.
fn import_plane(p: &mut Plane, src: &[u8], crop_w: usize, crop_h: usize) {
    for r in 0..crop_h {
        let dst = p.origin + r * p.stride;
        p.data[dst..dst + crop_w].copy_from_slice(&src[r * crop_w..r * crop_w + crop_w]);
        // Replicate the last column across the mi padding.
        let last = p.data[dst + crop_w - 1];
        for c in crop_w..p.width {
            p.data[dst + c] = last;
        }
    }
    // Replicate the last active row down across the mi padding.
    for r in crop_h..p.height {
        let (src_row, dst_row) = (p.origin + (crop_h - 1) * p.stride, p.origin + r * p.stride);
        p.data.copy_within(src_row..src_row + p.width, dst_row);
    }
}

fn export_plane(p: &Plane, crop_w: usize, crop_h: usize, out: &mut Vec<u8>) {
    for r in 0..crop_h {
        let src = p.origin + r * p.stride;
        out.extend_from_slice(&p.data[src..src + crop_w]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tight-packed I420 test pattern for `w`x`h`.
    fn make_i420(w: u32, h: u32) -> Vec<u8> {
        let (yw, yh) = (w as usize, h as usize);
        let (cw, ch) = ((w.div_ceil(2)) as usize, (h.div_ceil(2)) as usize);
        let mut v = Vec::new();
        for r in 0..yh {
            for c in 0..yw {
                v.push(((r * 7 + c * 3) & 0xff) as u8);
            }
        }
        for _ in 0..2 {
            for r in 0..ch {
                for c in 0..cw {
                    v.push(((r * 5 + c * 11) & 0xff) as u8);
                }
            }
        }
        v
    }

    #[test]
    fn roundtrip_even_dims() {
        let mut fb = FrameBuffer::new(64, 64);
        let src = make_i420(64, 64);
        fb.import_i420(&src, 64, 64).unwrap();
        assert_eq!(fb.export_i420(), src);
    }

    #[test]
    fn roundtrip_odd_dims() {
        // 636x476 is not a multiple of 8; the active region rounds to 640x480.
        let mut fb = FrameBuffer::new(636, 476);
        let src = make_i420(636, 476);
        fb.import_i420(&src, 636, 476).unwrap();
        assert_eq!(fb.export_i420(), src);
        assert_eq!(fb.mi_cols(), 80);
        assert_eq!(fb.mi_rows(), 60);
    }

    #[test]
    fn mi_padding_replicates_edge() {
        let mut fb = FrameBuffer::new(636, 476);
        let src = make_i420(636, 476);
        fb.import_i420(&src, 636, 476).unwrap();
        let (data, origin, stride, width, height) = fb.y();
        // Column 636..640 replicate column 635 on each active row.
        for r in 0..height {
            let last_real = data[origin + r * stride + 635];
            for c in 636..width {
                assert_eq!(data[origin + r * stride + c], last_real);
            }
        }
        // Rows 476..480 replicate row 475.
        for r in 476..height {
            for c in 0..width {
                assert_eq!(
                    data[origin + r * stride + c],
                    data[origin + 475 * stride + c]
                );
            }
        }
    }

    #[test]
    fn extend_borders_replicates_corners_and_edges() {
        let mut fb = FrameBuffer::new(16, 16);
        let src = make_i420(16, 16);
        fb.import_i420(&src, 16, 16).unwrap();
        fb.extend_borders();
        let (data, origin, stride, width, height) = fb.y();
        // Left border replicates column 0.
        for r in 0..height {
            let row = origin + r * stride;
            assert_eq!(data[row - 1], data[row]);
            assert_eq!(data[row - BORDER], data[row]);
        }
        // Right border replicates the last column.
        for r in 0..height {
            let row = origin + r * stride;
            assert_eq!(data[row + width], data[row + width - 1]);
        }
        // Top-left corner replicates sample (0,0).
        let tl = data[origin];
        assert_eq!(data[origin - stride - 1], tl);
        assert_eq!(data[origin - BORDER * stride - BORDER], tl);
        // Bottom border replicates the last active row.
        for c in 0..width {
            assert_eq!(
                data[origin + height * stride + c],
                data[origin + (height - 1) * stride + c]
            );
        }
    }

    #[test]
    fn import_rejects_wrong_size() {
        let mut fb = FrameBuffer::new(32, 32);
        let src = make_i420(16, 16);
        assert!(fb.import_i420(&src, 16, 16).is_err());
    }
}
