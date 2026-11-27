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

use crate::feature_flags::parse_bool_flag;
use lazy_static::lazy_static;
use std::env;

lazy_static! {
    // Feature flags, see `FeatureFlags`.
    pub static ref FEATURE_MEETING_MANAGEMENT: bool =
        parse_bool_flag(env::var("FEATURE_MEETING_MANAGEMENT").ok().as_deref());
    pub static ref DATABASE_ENABLED: bool =
        parse_bool_flag(env::var("DATABASE_ENABLED").ok().as_deref());
}
