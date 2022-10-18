// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub static VIDEO_CODEC: &str = "vp09.00.10.08";
// https://www.w3.org/TR/webcodecs-codec-registry/
pub static AUDIO_CODEC: &str = "opus";
pub const AUDIO_CHANNELS: u32 = 1u32;
pub const AUDIO_SAMPLE_RATE: u32 = 48000u32;

pub const VIDEO_HEIGHT: i32 = 720i32;
pub const VIDEO_WIDTH: i32 = 1280i32;
pub const ACTIX_WEBSOCKET: &'static str = concat!(
    "ws://",
    std::env!("ACTIX_HOST"),
    ":",
    std::env!("ACTIX_PORT"),
    "/lobby"
);
