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

//! libvpx-backed VP9 encoder.
//!
//! This wraps the C libvpx VP9 encoder (`env-libvpx-sys`). It is compiled only
//! on native targets with the `libvpx` feature, where it serves both as the
//! test oracle for the pure-Rust encoder and as an opt-in encode backend. It
//! implements the codec-agnostic [`Encodable`] trait so it is interchangeable
//! with [`crate::vp9::Vp9Encoder`].

use super::{Encodable, EncodedFrame, EncoderConfig};
use anyhow::Result;
use std::ffi::{c_int, CStr};
use std::mem::MaybeUninit;
use std::os::raw::c_char;
use vpx_sys::*;

const FPS: u64 = 30;

// --- VP9 Encoder Implementation (inspired by videocall-rs) ---

/// A safe wrapper for the vpx_codec_ctx_t
pub struct Vp9Encoder {
    ctx: vpx_codec_ctx_t,
    cfg: vpx_codec_enc_cfg_t,
    pub width: u32,
    pub height: u32,
}

// These are necessary because the context contains raw pointers.
// We are guaranteeing that we are using the encoder in a thread-safe way.
unsafe impl Send for Vp9Encoder {}
unsafe impl Sync for Vp9Encoder {}

/// Helper to convert C strings to Rust Strings for error messages.
fn c_str_to_rust_str(c_str_ptr: *const c_char) -> String {
    if c_str_ptr.is_null() {
        return "Unknown error".to_string();
    }
    unsafe { CStr::from_ptr(c_str_ptr).to_string_lossy().into_owned() }
}

impl Vp9Encoder {
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self> {
        unsafe {
            let mut cfg: vpx_codec_enc_cfg_t = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_config_default(vpx_codec_vp9_cx(), &mut cfg, 0);
            if ret != VPX_CODEC_OK {
                anyhow::bail!("Failed to get default VP9 encoder config");
            }

            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = FPS as c_int;
            cfg.rc_target_bitrate = bitrate_kbps;
            cfg.rc_end_usage = vpx_rc_mode::VPX_VBR;
            // Keyframe settings can be important for streaming
            cfg.kf_max_dist = 150;
            cfg.kf_min_dist = 150;
            cfg.kf_mode = vpx_kf_mode::VPX_KF_AUTO;

            let mut ctx: vpx_codec_ctx_t = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_init_ver(
                &mut ctx,
                vpx_codec_vp9_cx(),
                &cfg,
                0,
                VPX_ENCODER_ABI_VERSION as i32,
            );
            if ret != VPX_CODEC_OK {
                let err_msg = c_str_to_rust_str(vpx_codec_err_to_string(ret));
                anyhow::bail!("Failed to initialize VP9 encoder: {err_msg}");
            }

            Ok(Vp9Encoder {
                ctx,
                cfg,
                width,
                height,
            })
        }
    }

    pub fn encode(&mut self, frame_count: i64, yuv_data: Option<&[u8]>) -> Result<Frames<'_>> {
        unsafe {
            let image_ptr = if let Some(data) = yuv_data {
                let mut image: vpx_image_t = MaybeUninit::zeroed().assume_init();
                vpx_img_wrap(
                    &mut image,
                    vpx_img_fmt::VPX_IMG_FMT_I420,
                    self.width,
                    self.height,
                    1,
                    data.as_ptr() as *mut u8,
                );
                &image as *const _
            } else {
                // This is the flush call.
                std::ptr::null()
            };

            let ret = vpx_codec_encode(
                &mut self.ctx,
                image_ptr,
                frame_count, // pts
                1,           // duration
                0,
                VPX_DL_REALTIME as u64,
            );
            if ret != VPX_CODEC_OK {
                let err_msg = c_str_to_rust_str(vpx_codec_error(&mut self.ctx as *mut _));
                anyhow::bail!("Failed to encode frame: {err_msg:?}");
            }

            Ok(Frames {
                ctx: &mut self.ctx,
                iter: std::ptr::null(),
            })
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

// --- Frame iterator structures (from videocall-cli) ---

#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Compressed data.
    pub data: &'a [u8],
    /// Whether the frame is a keyframe.
    pub key: bool,
    /// Presentation timestamp (in timebase units).
    pub pts: i64,
}

pub struct Frames<'a> {
    ctx: &'a mut vpx_codec_ctx_t,
    iter: vpx_codec_iter_t,
}

impl<'a> Iterator for Frames<'a> {
    type Item = Frame<'a>;
    #[allow(clippy::unnecessary_cast)]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            unsafe {
                let pkt = vpx_codec_get_cx_data(self.ctx, &mut self.iter);
                if pkt.is_null() {
                    return None;
                } else if (*pkt).kind == vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    let f = &(*pkt).data.frame;
                    return Some(Frame {
                        data: std::slice::from_raw_parts(f.buf as _, f.sz as usize),
                        key: (f.flags & VPX_FRAME_IS_KEY) != 0,
                        pts: f.pts,
                    });
                }
            }
        }
    }
}

// --- Codec-agnostic `Encodable` implementation ---
//
// This mirrors the libvpx configuration used by `videocall-cli`'s
// `video_encoder.rs` (one-pass realtime VBR, error-resilient, CPUUSED 5, tile
// columns, row-mt, frame-parallel) so that this backend can drive the CLI's
// `--features libvpx` path with identical behaviour.

impl Encodable for Vp9Encoder {
    fn new(config: EncoderConfig) -> Result<Self> {
        unsafe {
            let mut cfg: vpx_codec_enc_cfg_t = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_config_default(vpx_codec_vp9_cx(), &mut cfg, 0);
            if ret != VPX_CODEC_OK {
                anyhow::bail!("Failed to get default VP9 encoder config");
            }

            cfg.g_w = config.width;
            cfg.g_h = config.height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = config.framerate as c_int;
            cfg.rc_target_bitrate = config.bitrate_kbps;
            cfg.rc_min_quantizer = config.min_quantizer;
            cfg.rc_max_quantizer = config.max_quantizer;
            cfg.g_threads = 2;
            cfg.g_lag_in_frames = 1;
            cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
            cfg.g_pass = vpx_enc_pass::VPX_RC_ONE_PASS;
            cfg.g_profile = 0;
            cfg.rc_end_usage = vpx_rc_mode::VPX_VBR;
            cfg.kf_max_dist = config.keyframe_interval;
            cfg.kf_min_dist = config.keyframe_interval;
            cfg.kf_mode = vpx_kf_mode::VPX_KF_AUTO;

            let mut ctx: vpx_codec_ctx_t = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_init_ver(
                &mut ctx,
                vpx_codec_vp9_cx(),
                &cfg,
                0,
                VPX_ENCODER_ABI_VERSION as i32,
            );
            if ret != VPX_CODEC_OK {
                let err_msg = c_str_to_rust_str(vpx_codec_err_to_string(ret));
                anyhow::bail!("Failed to initialize VP9 encoder: {err_msg}");
            }

            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, 5);
            vpx_codec_control_(
                &mut ctx,
                vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int,
                4,
            );
            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int, 1);
            vpx_codec_control_(
                &mut ctx,
                vp8e_enc_control_id::VP9E_SET_FRAME_PARALLEL_DECODING as c_int,
                1,
            );

            Ok(Vp9Encoder {
                ctx,
                cfg,
                width: config.width,
                height: config.height,
            })
        }
    }

    fn update_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        self.cfg.rc_target_bitrate = kbps;
        unsafe {
            let ret = vpx_codec_enc_config_set(&mut self.ctx, &self.cfg);
            if ret != VPX_CODEC_OK {
                let err_msg = c_str_to_rust_str(vpx_codec_error(&mut self.ctx as *mut _));
                anyhow::bail!("Failed to update encoder bitrate: {err_msg}");
            }
        }
        Ok(())
    }

    fn encode(&mut self, pts: i64, i420: &[u8]) -> Result<Option<EncodedFrame>> {
        unsafe {
            let mut image: vpx_image_t = MaybeUninit::zeroed().assume_init();
            vpx_img_wrap(
                &mut image,
                vpx_img_fmt::VPX_IMG_FMT_I420,
                self.width,
                self.height,
                1,
                i420.as_ptr() as *mut u8,
            );

            let ret = vpx_codec_encode(&mut self.ctx, &image, pts, 1, 0, VPX_DL_REALTIME as u64);
            if ret != VPX_CODEC_OK {
                let err_msg = c_str_to_rust_str(vpx_codec_error(&mut self.ctx as *mut _));
                anyhow::bail!("Failed to encode frame: {err_msg}");
            }

            let mut iter: vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    return Ok(None);
                }
                if (*pkt).kind == vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    let f = &(*pkt).data.frame;
                    let data =
                        std::slice::from_raw_parts(f.buf as *const u8, f.sz as usize).to_vec();
                    return Ok(Some(EncodedFrame {
                        data,
                        is_keyframe: (f.flags & VPX_FRAME_IS_KEY) != 0,
                        pts: f.pts,
                    }));
                }
            }
        }
    }
}
