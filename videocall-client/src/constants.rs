pub static AUDIO_CODEC: &str = "opus"; // https://www.w3.org/TR/webcodecs-codec-registry/#audio-codec-registry
pub static VIDEO_CODEC: &str = "vp09.00.10.08"; // profile 0,level 1.0, bit depth 8,

// Commented out because it is not as fast as vp9.

// pub static VIDEO_CODEC: &str = "av01.0.01M.08";
// av01: AV1
// 0 profile: main profile
// 01 level: level2.1
// M tier: Main tier
// 08 bit depth = 8 bits

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
