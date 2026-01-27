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

use crate::utils::is_firefox;
use videocall_types::protos::media_packet::VideoCodec;

pub static AUDIO_CODEC: &str = "opus"; // https://www.w3.org/TR/webcodecs-codec-registry/#audio-codec-registry

// VP8 codec string - used for Firefox (software VP9 is slow)
pub static VIDEO_CODEC_VP8: &str = "vp8";

// VP9 codec string - used for Chrome/Edge/Safari (often hardware accelerated)
pub static VIDEO_CODEC_VP9: &str = "vp09.00.10.08"; // profile 0, level 1.0, bit depth 8

/// Returns the appropriate video codec string based on the browser.
/// Firefox uses VP8 (software VP9 is too slow), others use VP9.
pub fn get_video_codec_string() -> &'static str {
    if is_firefox() {
        VIDEO_CODEC_VP8
    } else {
        VIDEO_CODEC_VP9
    }
}

/// Returns the VideoCodec enum value based on the browser.
pub fn get_video_codec() -> VideoCodec {
    if is_firefox() {
        VideoCodec::VP8
    } else {
        VideoCodec::VP9_PROFILE0_LEVEL10_8BIT
    }
}

// H.264 - requires description field in decoder config (SPS/PPS)
// pub static VIDEO_CODEC_H264: &str = "avc1.42001E"; // H.264 Baseline, Level 3.0

// AV1 - commented out because it is not as fast as vp9.
// pub static VIDEO_CODEC_AV1: &str = "av01.0.01M.08";

pub const AUDIO_CHANNELS: u32 = 1u32;
pub const AUDIO_SAMPLE_RATE: u32 = 48000u32;

pub const RSA_BITS: usize = 1024;

use videocall_types::protos::media_packet::media_packet::MediaType;

pub static SUPPORTED_MEDIA_TYPES: &[MediaType] = &[
    MediaType::AUDIO,
    MediaType::VIDEO,
    MediaType::SCREEN,
    MediaType::HEARTBEAT,
];
