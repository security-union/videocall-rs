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

use lazy_static::lazy_static;
use std::collections::HashMap;
use std::env;

lazy_static! {
    // Bot configuration, see `BotConfig::from_env_or_default`.
    pub static ref BOT_CONFIG_PATH: Option<String> = env::var("BOT_CONFIG_PATH").ok();
    pub static ref SERVER_URL: String = env::var("SERVER_URL")
        .unwrap_or_else(|_| "https://webtransport-us-east.webtransport.video".to_string());
    pub static ref N_CLIENTS: usize = env::var("N_CLIENTS")
        .unwrap_or_else(|_| "1".to_string())
        .parse::<usize>()
        .unwrap_or(1);
    pub static ref ROOM: String = env::var("ROOM").unwrap_or_else(|_| "test-room".to_string());
    pub static ref INSECURE: bool = env::var("INSECURE")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);
    /// Per-client `CLIENT_<i>_*` overrides, keyed by variable name.
    pub static ref CLIENT_ENV: HashMap<String, String> = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| key.starts_with("CLIENT_"))
        .collect();

    // Room token signing key, see `RoomTokenSigner::from_env`.
    pub static ref JWT_SECRET: Option<String> = env::var("JWT_SECRET").ok();
}
