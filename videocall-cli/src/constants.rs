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
    pub(crate) static ref HOSTNAME: Option<String> = env::var("HOSTNAME").ok();
    pub(crate) static ref COMPUTERNAME: Option<String> = env::var("COMPUTERNAME").ok();
    pub(crate) static ref SSH_CONNECTION: Option<String> = env::var("SSH_CONNECTION").ok();
    pub(crate) static ref DISPLAY: Option<String> = env::var("DISPLAY").ok();
    pub(crate) static ref WAYLAND_DISPLAY: Option<String> = env::var("WAYLAND_DISPLAY").ok();
    pub(crate) static ref VIDEOCALL_CONFIG_DIR: Option<String> = env::var(CONFIG_DIR_ENV).ok();
    pub(crate) static ref XDG_CONFIG_HOME: Option<String> = env::var("XDG_CONFIG_HOME").ok();
    pub(crate) static ref APPDATA: Option<String> = env::var("APPDATA").ok();
}

pub(crate) fn env_lookup(key: &str) -> Option<String> {
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
