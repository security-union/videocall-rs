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

//! Codec-agnostic video encoder interface.
//!
//! The [`Encodable`] trait abstracts over concrete encoder backends so that
//! consumers can encode I420 frames without depending on a particular codec or
//! implementation. Two backends implement it today:
//!
//! - [`crate::vp9::Vp9Encoder`] — the pure-Rust VP9 encoder (always compiled,
//!   including on `wasm32`).
//! - The legacy libvpx-backed `Vp9Encoder` in [`libvpx`], available only with
//!   the `libvpx` feature on native targets, used as a test oracle and as an
//!   opt-in encode backend.
//!
//! Both are interchangeable behind `Box<dyn Encodable>`.

use anyhow::Result;

// The legacy libvpx encoder is only available on native targets with the
// `libvpx` feature. `Vp9Encoder` is re-exported under the same cfg so
// `videocall_codecs::encoder::Vp9Encoder` keeps resolving for the native bins.
#[cfg(all(feature = "libvpx", not(target_arch = "wasm32")))]
pub mod libvpx;
#[cfg(all(feature = "libvpx", not(target_arch = "wasm32")))]
pub use libvpx::Vp9Encoder;

/// Configuration for a video encoder, independent of the concrete backend.
///
/// Backends map these fields onto their native configuration. Fields that a
/// given backend does not support are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Frame width in pixels. Must be even.
    pub width: u32,
    /// Frame height in pixels. Must be even.
    pub height: u32,
    /// Nominal frame rate in frames per second (the encoder time base is 1/framerate).
    pub framerate: u32,
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Maximum distance between keyframes, in frames.
    pub keyframe_interval: u32,
    /// Minimum quantizer (0-63); lower means higher quality.
    pub min_quantizer: u32,
    /// Maximum quantizer (0-63); higher means lower quality.
    pub max_quantizer: u32,
    /// Speed/quality trade-off (0 = slowest/best, 8 = fastest/worst).
    pub cpu_used: u8,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            framerate: 30,
            bitrate_kbps: 500,
            keyframe_interval: 150,
            min_quantizer: 40,
            max_quantizer: 60,
            cpu_used: 7,
        }
    }
}

/// A single compressed frame produced by an encoder.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Compressed bitstream for this frame (owned; consumers copy immediately).
    pub data: Vec<u8>,
    /// Whether this frame is a keyframe (decodable without references).
    pub is_keyframe: bool,
    /// Presentation timestamp in time-base units (as supplied to [`Encodable::encode`]).
    pub pts: i64,
}

/// A codec-agnostic video encoder.
///
/// Implementations accept planar I420 frames and emit at most one compressed
/// frame per call. Because some backends buffer frames (e.g. `lag_in_frames`),
/// [`encode`](Encodable::encode) may return `None` for a call that produced no
/// output yet; callers must tolerate zero-frame results.
pub trait Encodable {
    /// Create a new encoder with the given configuration.
    fn new(config: EncoderConfig) -> Result<Self>
    where
        Self: Sized;

    /// Update the target bitrate at runtime.
    fn update_bitrate_kbps(&mut self, kbps: u32) -> Result<()>;

    /// Encode one planar I420 frame.
    ///
    /// `pts` is the presentation timestamp in time-base units. `i420` is the
    /// full I420 buffer (`width * height * 3 / 2` bytes). Returns the compressed
    /// frame if one is ready, or `None` if the encoder buffered it.
    fn encode(&mut self, pts: i64, i420: &[u8]) -> Result<Option<EncodedFrame>>;
}

/// Construct an encoder for the given codec.
///
/// Currently only [`VideoCodec::Vp9Profile0Level10Bit8`](crate::decoder::VideoCodec::Vp9Profile0Level10Bit8)
/// is supported, backed by the pure-Rust [`crate::vp9::Vp9Encoder`]. Any other
/// codec returns an error.
pub fn create_encoder(
    codec: crate::decoder::VideoCodec,
    cfg: EncoderConfig,
) -> Result<Box<dyn Encodable + Send>> {
    use crate::decoder::VideoCodec;
    match codec {
        VideoCodec::Vp9Profile0Level10Bit8 => Ok(Box::new(crate::vp9::Vp9Encoder::new(cfg)?)),
        other => Err(anyhow::anyhow!(
            "no pure-Rust encoder available for codec {other:?}"
        )),
    }
}
