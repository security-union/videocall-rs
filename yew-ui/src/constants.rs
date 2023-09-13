// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub static AUDIO_CODEC: &str = "opus"; // https://www.w3.org/TR/webcodecs-codec-registry/#audio-codec-registry
pub static VIDEO_CODEC: &str = "vp09.02.10.12"; // profile 0,level 1.0, bit depth 8,

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
pub const ACTIX_WEBSOCKET: &str = concat!(std::env!("ACTIX_UI_BACKEND_URL"), "/lobby");
pub const WEBTRANSPORT_HOST: &str = concat!(std::env!("WEBTRANSPORT_HOST"), "/lobby");

pub const RSA_BITS: usize = 1024;

pub fn truthy(s: Option<&str>) -> bool {
    if let Some(s) = s {
        ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
    } else {
        false
    }
}
pub fn split_users(s: Option<&str>) -> Vec<String> {
    if let Some(s) = s {
        s.split(',').map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    }
}
// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    pub static ref ENABLE_OAUTH: bool = truthy(std::option_env!("ENABLE_OAUTH"));
    pub static ref WEBTRANSPORT_ENABLED: bool = truthy(std::option_env!("WEBTRANSPORT_ENABLED"));
    pub static ref E2EE_ENABLED: bool = truthy(std::option_env!("E2EE_ENABLED"));
    pub static ref USERS_ALLOWED_TO_STREAM: Vec<String> =
        split_users(std::option_env!("USERS_ALLOWED_TO_STREAM"));
}
