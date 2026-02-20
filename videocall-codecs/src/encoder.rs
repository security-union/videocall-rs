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

//! VP9 encoder using libvpx.
//!
//! This module provides a safe, builder-pattern API for VP9 encoding suitable
//! for real-time streaming.  It is the **single source of truth** for VP9
//! encoding across all native clients (bot, CLI, embedded).
//!
//! # Example
//!
//! ```no_run
//! use videocall_codecs::encoder::{VideoEncoderBuilder, VideoEncoder};
//!
//! let mut encoder = VideoEncoderBuilder::new(30, 5)
//!     .set_resolution(1280, 720)
//!     .set_bitrate_kbps(500)
//!     .build()
//!     .unwrap();
//!
//! // Encode an I420 frame
//! # let i420_data = vec![0u8; 1280 * 720 * 3 / 2];
//! for frame in encoder.encode(0, &i420_data).unwrap() {
//!     println!("encoded {} bytes, key={}", frame.data.len(), frame.key);
//! }
//! ```

use anyhow::{anyhow, Result};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_ulong};
use vpx_sys::*;

// ---------------------------------------------------------------------------
// Helper macros
// ---------------------------------------------------------------------------

macro_rules! vpx {
    ($f:expr) => {{
        let res = unsafe { $f };
        let res_int = unsafe { std::mem::transmute::<vpx_sys::vpx_codec_err_t, i32>(res) };
        if res_int != 0 {
            return Err(anyhow!("vpx function error code ({}).", res_int));
        }
        res
    }};
}

macro_rules! vpx_ptr {
    ($f:expr) => {{
        let res = unsafe { $f };
        if res.is_null() {
            return Err(anyhow!("vpx function returned null pointer."));
        }
        res
    }};
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for constructing a [`VideoEncoder`] with fine-grained control over
/// VP9 encoding parameters.
pub struct VideoEncoderBuilder {
    /// Minimum quantizer (lower = higher quality, 0-63).
    pub min_quantizer: u32,
    /// Maximum quantizer (higher = more compression, 0-63).
    pub max_quantizer: u32,
    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Frames per second.
    pub fps: u32,
    /// Resolution as (width, height). Both must be even and non-zero.
    pub resolution: (u32, u32),
    /// CPU usage / speed trade-off (0 = slowest/best, 15 = fastest/worst).
    /// Values 4–8 are recommended for real-time streaming.
    pub cpu_used: u32,
    /// VP9 encoding profile (0 = 8-bit 4:2:0, default).
    pub profile: u32,
}

impl VideoEncoderBuilder {
    /// Create a new builder with sensible defaults for real-time streaming.
    ///
    /// # Arguments
    /// * `fps` — target frame rate
    /// * `cpu_used` — speed vs quality (4–8 recommended, higher = faster)
    pub fn new(fps: u32, cpu_used: u8) -> Self {
        Self {
            bitrate_kbps: 500,
            max_quantizer: 60,
            min_quantizer: 40,
            resolution: (640, 480),
            fps,
            cpu_used: cpu_used as u32,
            profile: 0,
        }
    }

    /// Set the video resolution. Both width and height must be even and > 0.
    pub fn set_resolution(mut self, width: u32, height: u32) -> Self {
        self.resolution = (width, height);
        self
    }

    /// Set the target bitrate in kbps.
    pub fn set_bitrate_kbps(mut self, bitrate_kbps: u32) -> Self {
        self.bitrate_kbps = bitrate_kbps;
        self
    }

    /// Set the minimum quantizer (0-63, lower = better quality).
    pub fn set_min_quantizer(mut self, min: u32) -> Self {
        self.min_quantizer = min;
        self
    }

    /// Set the maximum quantizer (0-63, higher = more compression).
    pub fn set_max_quantizer(mut self, max: u32) -> Self {
        self.max_quantizer = max;
        self
    }

    /// Build the encoder. Returns an error if the resolution is invalid.
    pub fn build(&self) -> Result<VideoEncoder> {
        let (width, height) = self.resolution;
        if width % 2 != 0 || width == 0 {
            return Err(anyhow!("Width must be divisible by 2"));
        }
        if height % 2 != 0 || height == 0 {
            return Err(anyhow!("Height must be divisible by 2"));
        }

        let cfg_ptr = vpx_ptr!(vpx_codec_vp9_cx());
        let mut cfg = unsafe { MaybeUninit::zeroed().assume_init() };
        vpx!(vpx_codec_enc_config_default(cfg_ptr, &mut cfg, 0));

        cfg.g_w = width;
        cfg.g_h = height;
        cfg.g_timebase.num = 1;
        cfg.g_timebase.den = self.fps as c_int;
        cfg.rc_target_bitrate = self.bitrate_kbps;
        cfg.rc_min_quantizer = self.min_quantizer;
        cfg.rc_max_quantizer = self.max_quantizer;
        cfg.g_threads = 2;
        cfg.g_lag_in_frames = 1;
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.g_pass = vpx_enc_pass::VPX_RC_ONE_PASS;
        cfg.g_profile = self.profile;
        cfg.rc_end_usage = vpx_rc_mode::VPX_VBR;
        cfg.kf_max_dist = 150;
        cfg.kf_min_dist = 150;
        cfg.kf_mode = vpx_kf_mode::VPX_KF_AUTO;

        let mut ctx = unsafe { MaybeUninit::zeroed().assume_init() };

        vpx!(vpx_codec_enc_init_ver(
            &mut ctx,
            cfg_ptr,
            &cfg,
            0,
            VPX_ENCODER_ABI_VERSION as i32
        ));

        unsafe {
            vpx_codec_control_(
                &mut ctx,
                vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
                self.cpu_used as c_int,
            );
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
        }

        Ok(VideoEncoder {
            ctx,
            cfg,
            width,
            height,
        })
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// A VP9 video encoder wrapping libvpx.
///
/// Create via [`VideoEncoderBuilder::build()`].
pub struct VideoEncoder {
    ctx: vpx_codec_ctx_t,
    cfg: vpx_codec_enc_cfg_t,
    width: u32,
    height: u32,
}

// SAFETY: The encoder is used from a single thread at a time (send pattern).
unsafe impl Send for VideoEncoder {}
unsafe impl Sync for VideoEncoder {}

impl VideoEncoder {
    /// Dynamically update the target bitrate (kbps) without re-creating the encoder.
    pub fn update_bitrate_kbps(&mut self, bitrate: u32) -> Result<()> {
        self.cfg.rc_target_bitrate = bitrate;
        vpx!(vpx_codec_enc_config_set(&mut self.ctx, &self.cfg));
        Ok(())
    }

    /// Encode a single I420 frame.
    ///
    /// # Arguments
    /// * `pts` — presentation timestamp (in timebase units, i.e. frame count)
    /// * `data` — raw I420 pixel data (`width * height * 3 / 2` bytes)
    ///
    /// Returns an iterator of compressed [`Frame`]s (usually 0 or 1).
    pub fn encode(&mut self, pts: i64, data: &[u8]) -> Result<Frames<'_>> {
        let mut image = unsafe { MaybeUninit::zeroed().assume_init() };

        vpx_ptr!(vpx_img_wrap(
            &mut image,
            vpx_img_fmt::VPX_IMG_FMT_I420,
            self.width as _,
            self.height as _,
            1,
            data.as_ptr() as _,
        ));

        vpx!(vpx_codec_encode(
            &mut self.ctx,
            &image,
            pts,
            1, // duration
            0, // flags
            VPX_DL_REALTIME as c_ulong,
        ));

        Ok(Frames {
            ctx: &mut self.ctx,
            iter: std::ptr::null(),
        })
    }

    /// Current encoder width.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Current encoder height.
    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

/// A single compressed video frame produced by the encoder.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Compressed VP9 data.
    pub data: &'a [u8],
    /// Whether this is a keyframe (IDR).
    pub key: bool,
    /// Presentation timestamp (in timebase units).
    pub pts: i64,
}

/// Iterator over compressed frames produced by a single [`VideoEncoder::encode`] call.
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
