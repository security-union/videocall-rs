use lazy_static::lazy_static;

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
use videocall_types::truthy;

pub const CANVAS_LIMIT: usize = 20;

lazy_static! {
    pub static ref LOGIN_URL: String = 
        std::env::var("LOGIN_URL").unwrap_or_else(|_| "http://localhost:8081/login".to_string());
    pub static ref ACTIX_WEBSOCKET: String = format!(
        "{}/lobby",
        std::env::var("ACTIX_UI_BACKEND_URL").unwrap_or_else(|_| "ws://localhost:8081".to_string())
    );
    pub static ref WEBTRANSPORT_HOST: String = format!(
        "{}/lobby",
        std::env::var("WEBTRANSPORT_HOST").unwrap_or_else(|_| "https://127.0.0.1:4433".to_string())
    );
}

pub const AUDIO_BITRATE_KBPS: u32 = 65u32;
pub const VIDEO_BITRATE_KBPS: u32 = 1_000u32;
pub const SCREEN_BITRATE_KBPS: u32 = 1_000u32;

pub fn split_users(s: Option<&str>) -> Vec<String> {
    if let Some(s) = s {
        s.split(',')
            .filter_map(|s| {
                let s = s.trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect::<Vec<String>>()
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
