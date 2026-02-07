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

mod webmedia;

// Yew-compat feature gates all connection-related modules that depend on yew-websocket/yew-webtransport
#[cfg(feature = "yew-compat")]
#[allow(clippy::module_inception)]
mod connection;
#[cfg(feature = "yew-compat")]
mod connection_controller;
#[cfg(feature = "yew-compat")]
mod connection_manager;
#[cfg(feature = "yew-compat")]
mod task;
#[cfg(feature = "yew-compat")]
mod websocket;
#[cfg(feature = "yew-compat")]
mod webtransport;

#[cfg(feature = "yew-compat")]
pub use connection_controller::ConnectionController;
#[cfg(feature = "yew-compat")]
pub use connection_manager::{ConnectionManagerOptions, ConnectionState};
pub use webmedia::ConnectOptions;
