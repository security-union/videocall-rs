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

#[allow(clippy::module_inception)]
mod connection;
mod connection_manager;
mod task;
mod webmedia;
mod websocket;
mod webtransport;

pub use connection_manager::{ConnectionManager, ConnectionManagerOptions, ConnectionState};
pub use webmedia::ConnectOptions;
