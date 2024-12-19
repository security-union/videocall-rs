use anyhow::{anyhow, Result};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_ulong};
use tracing::info;
use vpx_sys::*;

macro_rules! vpx {
    ($f:expr) => {{
        let res = unsafe { $f };
        let res_int = unsafe { std::mem::transmute::<_, i32>(res) };
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
    pub timebase: (c_int, c_int),
    pub resolution: (u32, u32),
    pub cpu_used: u32,
    pub profile: u32,
}

impl Default for VideoEncoderBuilder {
    fn default() -> Self {
        Self {
            bitrate_kbps: 200_000,
            max_quantizer: 63,
            min_quantizer: 25,
            resolution: (640, 480),
            timebase: (1, 1000),
            cpu_used: 5,
            profile: 0,
        }
    }
}

impl VideoEncoderBuilder {
    pub fn set_resolution(mut self, width: u32, height: u32) -> Self {
        info!("Setting resolution to {}x{}", width, height);
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

        let (num, den) = self.timebase;

        cfg.g_w = width;
        cfg.g_h = height;
        cfg.g_timebase.num = num;
        cfg.g_timebase.den = den;
        cfg.rc_target_bitrate = self.bitrate_kbps;
        cfg.rc_min_quantizer = self.min_quantizer;
        cfg.rc_max_quantizer = self.max_quantizer;
        cfg.g_threads = 2;
        cfg.g_lag_in_frames = 1;
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.g_pass = vpx_enc_pass::VPX_RC_ONE_PASS;
        cfg.g_profile = self.profile;
        cfg.rc_end_usage = vpx_rc_mode::VPX_Q;
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
        // set encoder internal speed settings
        vpx!(vpx_codec_control_(
            &mut ctx,
            vp8e_enc_control_id::VP8E_SET_CPUUSED as _,
            self.cpu_used as c_int
        ));
        // set row level multi-threading
        vpx!(vpx_codec_control_(
            &mut ctx,
            vp8e_enc_control_id::VP9E_SET_ROW_MT as _,
            1 as c_int
        ));
        vpx!(vpx_codec_control_(
            &mut ctx,
            vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as _,
            4 as c_int
        ));
        vpx!(vpx_codec_control_(
            &mut ctx,
            vp8e_enc_control_id::VP9E_SET_MAX_INTER_BITRATE_PCT as _,
            50 as c_int
        ));
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
    pub fn update_bitrate(&mut self, bitrate: u32) -> anyhow::Result<()> {
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
