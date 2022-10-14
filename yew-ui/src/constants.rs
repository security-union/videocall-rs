// This is read at compile time, please restart if you change this value.
pub const ACTIX_PORT: &str = std::env!("ACTIX_PORT");
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub static VIDEO_CODEC: &str = "vp09.00.10.08";
pub const VIDEO_HEIGHT: i32 = 360i32;
pub const VIDEO_WIDTH: i32 = 640i32;
pub const ACTIX_WEBSOCKET: &'static str =
    concat!("ws://localhost:", std::env!("ACTIX_PORT"), "/lobby");
