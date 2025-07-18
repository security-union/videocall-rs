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

// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub const ACTIX_WEBSOCKET: &str = std::env!("ACTIX_UI_BACKEND_URL");
pub const WEBTRANSPORT_HOST: &str = std::env!("WEBTRANSPORT_HOST");
pub const CANVAS_LIMIT: usize = 20;

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
    pub static ref SERVER_ELECTION_PERIOD_MS: u64 = std::option_env!("SERVER_ELECTION_PERIOD_MS")
        .unwrap_or("2000")
        .parse()
        .expect("Failed to parse SERVER_ELECTION_PERIOD_MS");
}
