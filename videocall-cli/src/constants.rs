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

use crate::auth::store::CONFIG_DIR_ENV;
use lazy_static::lazy_static;
use std::env;

lazy_static! {
    // Default device name, see `auth::hostname`.
    pub static ref HOSTNAME: Option<String> = env::var("HOSTNAME").ok();
    pub static ref COMPUTERNAME: Option<String> = env::var("COMPUTERNAME").ok();

    // Whether a browser can open here, see `auth::browser_expected`.
    pub static ref SSH_CONNECTION: Option<String> = env::var("SSH_CONNECTION").ok();
    pub static ref DISPLAY: Option<String> = env::var("DISPLAY").ok();
    pub static ref WAYLAND_DISPLAY: Option<String> = env::var("WAYLAND_DISPLAY").ok();

    // Config directory, see `auth::store::config_dir_from`.
    pub static ref VIDEOCALL_CONFIG_DIR: Option<String> = env::var(CONFIG_DIR_ENV).ok();
    pub static ref XDG_CONFIG_HOME: Option<String> = env::var("XDG_CONFIG_HOME").ok();
    pub static ref APPDATA: Option<String> = env::var("APPDATA").ok();
}

/// The statics above by variable name, for helpers that take an env lookup.
/// Any other name reads as unset.
pub fn env_lookup(key: &str) -> Option<String> {
    match key {
        "SSH_CONNECTION" => SSH_CONNECTION.clone(),
        "DISPLAY" => DISPLAY.clone(),
        "WAYLAND_DISPLAY" => WAYLAND_DISPLAY.clone(),
        CONFIG_DIR_ENV => VIDEOCALL_CONFIG_DIR.clone(),
        "XDG_CONFIG_HOME" => XDG_CONFIG_HOME.clone(),
        "APPDATA" => APPDATA.clone(),
        _ => {
            debug_assert!(false, "env_lookup: no static for {key}");
            None
        }
    }
}
