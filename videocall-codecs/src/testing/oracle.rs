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

//! Synchronous libvpx VP9 decode oracle for the encoder test harness.
//!
//! Unlike [`crate::decoder`]'s threaded `NativeDecoder`, this is a plain
//! blocking wrapper around `vpx_codec_vp9_dx`: encode a frame with the
//! pure-Rust encoder, feed the bitstream here, and assert on the decoded
//! pixels. Kept deliberately simple and single-threaded for test determinism.

use anyhow::{anyhow, Result};
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::ptr;
use vpx_sys::{
    vpx_codec_ctx_t, vpx_codec_dec_init_ver, vpx_codec_decode, vpx_codec_destroy,
    vpx_codec_err_to_string, vpx_codec_get_frame, vpx_codec_vp9_dx, vpx_image_t, VPX_CODEC_OK,
    VPX_DECODER_ABI_VERSION,
};

/// A single frame decoded by the oracle, as planar I420.
#[derive(Debug, Clone)]
pub struct OracleFrame {
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Contiguous I420 planes (Y, then U, then V).
    pub i420: Vec<u8>,
}

/// A blocking libvpx VP9 decoder. Destroys its codec context on drop.
pub struct OracleDecoder {
    ctx: vpx_codec_ctx_t,
}

impl OracleDecoder {
    /// Initialize a fresh VP9 decoder context.
    pub fn new() -> Result<Self> {
        let mut ctx: vpx_codec_ctx_t = unsafe { MaybeUninit::zeroed().assume_init() };
        let ret = unsafe {
            vpx_codec_dec_init_ver(
                &mut ctx,
                vpx_codec_vp9_dx(),
                ptr::null_mut(),
                0,
                VPX_DECODER_ABI_VERSION as i32,
            )
        };
        if ret != VPX_CODEC_OK {
            return Err(anyhow!(
                "failed to init VP9 oracle decoder: {}",
                err_str(ret)
            ));
        }
        Ok(Self { ctx })
    }

    /// Decode one compressed frame, returning every image it emitted (usually
    /// zero or one).
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<OracleFrame>> {
        let ret = unsafe {
            vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as u32,
                ptr::null_mut(),
                0,
            )
        };
        if ret != VPX_CODEC_OK {
            return Err(anyhow!("VP9 oracle decode failed: {}", err_str(ret)));
        }

        let mut frames = Vec::new();
        let mut iter = ptr::null_mut::<c_void>();
        loop {
            let img = unsafe {
                vpx_codec_get_frame(&mut self.ctx, &mut iter as *mut _ as *mut *const c_void)
            };
            if img.is_null() {
                break;
            }
            frames.push(unsafe { copy_image(img) });
        }
        Ok(frames)
    }
}

impl Drop for OracleDecoder {
    fn drop(&mut self) {
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

/// Copy a `vpx_image_t` into a contiguous I420 [`OracleFrame`], honoring stride.
unsafe fn copy_image(img: *const vpx_image_t) -> OracleFrame {
    let width = (*img).d_w as usize;
    let height = (*img).d_h as usize;
    let uv_width = width.div_ceil(2);
    let uv_height = height.div_ceil(2);

    let mut i420 = Vec::with_capacity(width * height + 2 * uv_width * uv_height);
    copy_plane((*img).planes[0], (*img).stride[0], width, height, &mut i420);
    copy_plane(
        (*img).planes[1],
        (*img).stride[1],
        uv_width,
        uv_height,
        &mut i420,
    );
    copy_plane(
        (*img).planes[2],
        (*img).stride[2],
        uv_width,
        uv_height,
        &mut i420,
    );

    OracleFrame {
        width: width as u32,
        height: height as u32,
        i420,
    }
}

/// Copy a single plane row by row into `buffer`, skipping stride padding.
unsafe fn copy_plane(
    plane: *const u8,
    stride: i32,
    width: usize,
    height: usize,
    buffer: &mut Vec<u8>,
) {
    let mut row = plane;
    for _ in 0..height {
        buffer.extend_from_slice(std::slice::from_raw_parts(row, width));
        row = row.offset(stride as isize);
    }
}

/// Render a libvpx error code as a human-readable string.
fn err_str(code: vpx_sys::vpx_codec_err_t) -> String {
    unsafe {
        let ptr = vpx_codec_err_to_string(code);
        if ptr.is_null() {
            "unknown codec error".to_string()
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
