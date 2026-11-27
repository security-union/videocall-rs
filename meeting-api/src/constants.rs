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
 */

use lazy_static::lazy_static;
use std::env;

lazy_static! {
    pub(crate) static ref DEVICE_TOKEN_AUDIENCE: Option<String> =
        env::var("DEVICE_TOKEN_AUDIENCE").ok();
    pub(crate) static ref AFTER_LOGIN_URL: Option<String> = env::var("AFTER_LOGIN_URL").ok();
}
