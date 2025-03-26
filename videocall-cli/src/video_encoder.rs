use anyhow::{anyhow, Result};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_ulong};
use vpx_sys::*;

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

pub struct VideoEncoderBuilder {
    pub min_quantizer: u32,
    pub max_quantizer: u32,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub resolution: (u32, u32),
    pub cpu_used: u32,
    pub profile: u32,
}

impl VideoEncoderBuilder {
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
}

impl VideoEncoderBuilder {
    pub fn set_resolution(mut self, width: u32, height: u32) -> Self {
        self.resolution = (width, height);
        self
    }

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

        let ctx = MaybeUninit::zeroed();
        let mut ctx = unsafe { ctx.assume_init() };

        vpx!(vpx_codec_enc_init_ver(
            &mut ctx,
            cfg_ptr,
            &cfg,
            0,
            VPX_ENCODER_ABI_VERSION as i32
        ));
        unsafe {
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
            // vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP9E_SET_AQ_MODE as c_int, 3);
        }
        Ok(VideoEncoder {
            ctx,
            cfg,
            width: self.resolution.0,
            height: self.resolution.1,
        })
    }
}

pub struct VideoEncoder {
    ctx: vpx_codec_ctx_t,
    cfg: vpx_codec_enc_cfg_t,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    pub fn update_bitrate_kbps(&mut self, bitrate: u32) -> anyhow::Result<()> {
        self.cfg.rc_target_bitrate = bitrate;
        vpx!(vpx_codec_enc_config_set(&mut self.ctx, &self.cfg));
        Ok(())
    }

    pub fn encode(&mut self, pts: i64, data: &[u8]) -> anyhow::Result<Frames> {
        let image = MaybeUninit::zeroed();
        let mut image = unsafe { image.assume_init() };

        vpx_ptr!(vpx_img_wrap(
            &mut image,
            vpx_img_fmt::VPX_IMG_FMT_I420,
            self.width as _,
            self.height as _,
            1,
            data.as_ptr() as _,
        ));

        let flags: i64 = 0;

        vpx!(vpx_codec_encode(
            &mut self.ctx,
            &image,
            pts,
            1,     // Duration
            flags, // Flags
            VPX_DL_REALTIME as c_ulong,
        ));

        Ok(Frames {
            ctx: &mut self.ctx,
            iter: std::ptr::null(),
        })
    }
}

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
