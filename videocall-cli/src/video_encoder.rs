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

//! VP9 video encoder adapter.
//!
//! This is a thin, backend-generic wrapper over a [`videocall_codecs`] encoder
//! that implements the [`Encodable`] trait. The concrete backend is chosen at
//! compile time:
//!
//! - Default build (no `libvpx` feature): the pure-Rust
//!   [`videocall_codecs::vp9::Vp9Encoder`] — zero C dependencies.
//! - `--features libvpx`: the C libvpx-backed encoder, opt-in for comparison.
//!
//! The public API (`VideoEncoderBuilder`, `VideoEncoder`, `Frame`, `Frames`) is
//! identical across both backends, so downstream code (`encoder_thread`,
//! `camera`) is unaffected by the choice.

use anyhow::{anyhow, Result};
use videocall_codecs::encoder::{Encodable, EncodedFrame, EncoderConfig};

/// Maximum distance between keyframes, in frames (`kf_max_dist`/`kf_min_dist` = 150).
const KEYFRAME_INTERVAL: u32 = 150;

// Backend selection. The default (C-free) path uses the pure-Rust encoder; the
// `libvpx` feature swaps in the C wrapper. Both are used through the
// `Encodable` trait, so the rest of this file is backend-agnostic.
#[cfg(feature = "libvpx")]
use videocall_codecs::encoder::Vp9Encoder as Inner;
#[cfg(not(feature = "libvpx"))]
use videocall_codecs::vp9::Vp9Encoder as Inner;

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

        let config = EncoderConfig {
            width,
            height,
            framerate: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            keyframe_interval: KEYFRAME_INTERVAL,
            min_quantizer: self.min_quantizer,
            max_quantizer: self.max_quantizer,
            cpu_used: self.cpu_used as u8,
        };

        // Fully-qualified so we always hit the `Encodable` trait method: the
        // libvpx backend also has an inherent `new`/`encode` with a
        // different signature that would otherwise shadow the trait.
        let inner = <Inner as Encodable>::new(config)?;
        Ok(VideoEncoder {
            inner,
            pending: None,
        })
    }
}

pub struct VideoEncoder {
    inner: Inner,
    /// Owns the most recently encoded frame so [`Frames`] can borrow it. Some
    /// backends buffer (libvpx `lag_in_frames`), so this may be `None` for a
    /// call that produced no output yet.
    pending: Option<EncodedFrame>,
}

impl VideoEncoder {
    pub fn update_bitrate_kbps(&mut self, bitrate: u32) -> anyhow::Result<()> {
        Encodable::update_bitrate_kbps(&mut self.inner, bitrate)
    }

    pub fn encode(&mut self, pts: i64, data: &[u8]) -> anyhow::Result<Frames<'_>> {
        self.pending = Encodable::encode(&mut self.inner, pts, data)?;
        Ok(Frames {
            frame: self.pending.as_ref().map(|f| Frame {
                data: &f.data,
                key: f.is_keyframe,
                pts: f.pts,
            }),
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

/// Iterator over the frames produced by a single [`VideoEncoder::encode`] call.
///
/// The backend emits at most one compressed frame per input frame, so this
/// yields either zero or one [`Frame`]. Downstream code already tolerates a
/// zero-frame result (the libvpx backend buffers with `lag_in_frames = 1`).
pub struct Frames<'a> {
    frame: Option<Frame<'a>>,
}

impl<'a> Iterator for Frames<'a> {
    type Item = Frame<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.frame.take()
    }
}
