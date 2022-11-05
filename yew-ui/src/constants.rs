// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub static VIDEO_CODEC: &str = "vp09.00.10.08"; // profile 0,level 1.0, bit depth 8,

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

fn truthy(s: String) -> bool {
    ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
}
// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    pub static ref ENABLE_OAUTH: bool = truthy(std::env!("ENABLE_OAUTH").to_string());
}
