// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub static VIDEO_CODEC: &str = "vp09.00.10.08";
pub const VIDEO_HEIGHT: i32 = 720i32;
pub const VIDEO_WIDTH: i32 = 1280i32;
pub const ACTIX_WEBSOCKET: &'static str = concat!(
    "ws://",
    std::env!("ACTIX_HOST"),
    ":",
    std::env!("ACTIX_PORT"),
    "/lobby"
);
