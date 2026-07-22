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
// Raised from 256 (#1660): the large lazy_static! metrics block expands recursively (one level
// per static ref), and adding the screen playout-family gauges tipped histogram_opts! macro
// expansion past 256. Compile-time only; no runtime effect.
#![recursion_limit = "512"]

pub mod actors;
pub mod auth;
pub mod client_diagnostics;
pub mod constants;
pub mod db;
pub mod lobby;
pub mod messages;
pub mod metrics;
pub mod models;
pub mod server_diagnostics;
pub mod session_manager;
pub mod token_validator;
pub mod version;
pub mod webtransport;
