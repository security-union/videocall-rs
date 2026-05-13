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
mod connection_controller;
mod connection_lost_reason;
mod connection_manager;
mod task;
mod url_log;
mod webmedia;
mod websocket;
mod webtransport;

// Phase 3b (discussion #793). Compiled in only when the `netsim`
// feature is on; production builds skip this module entirely so the
// send paths are byte-for-byte equivalent to pre-3b.
#[cfg(feature = "netsim")]
mod netsim_hook;

pub use connection_controller::ConnectionController;
pub use connection_lost_reason::ConnectionLostReason;
#[allow(unused_imports)]
pub use connection_manager::ReconnectionPhase;
pub use connection_manager::{
    connection_handshake_failures, connection_session_drops, ConnectionManagerOptions,
    ConnectionState,
};
pub use webmedia::{ConnectOptions, MediaStreamKey};
