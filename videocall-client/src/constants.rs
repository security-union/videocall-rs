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
pub const AUDIO_BITRATE: f64 = 50000f64;

// vga resolution
// pub const VIDEO_HEIGHT: i32 = 480i32;
// pub const VIDEO_WIDTH: i32 = 640i32;

pub const VIDEO_HEIGHT: i32 = 720i32;
pub const VIDEO_WIDTH: i32 = 1280i32;
pub const SCREEN_HEIGHT: u32 = 1080u32;
pub const SCREEN_WIDTH: u32 = 1920u32;

pub const RSA_BITS: usize = 1024;
