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

// Phase 3c (discussion #793). URL-param shim that reads
// `?netsim=<profile>` from `window.location` and installs the
// matching `NetSimShim` in `netsim_hook`. Compile-gated identically
// to the hook itself so default builds compile this out entirely.
#[cfg(feature = "netsim")]
mod netsim_url;

// Issue #1080. Runtime JS control surface (`window.__vcNetsim`) so the
// Playwright harness can install / clear netsim shaping mid-call. Same
// compile-gate as the rest of the netsim plumbing.
#[cfg(feature = "netsim")]
mod netsim_control;

pub use connection_controller::ConnectionController;
pub use connection_lost_reason::ConnectionLostReason;
#[allow(unused_imports)]
pub use connection_manager::ReconnectionPhase;
pub use connection_manager::{
    connection_handshake_failures, connection_session_drops, reelection_aborted_total,
    reelection_failed_total, reelection_preserved_total, reelection_proceeded_total,
    ConnectionManagerOptions, ConnectionState,
};
pub use webmedia::{ConnectOptions, MediaStreamKey};

// Issue #1080: the runtime netsim control-surface installer, re-exported
// so the UI crate (e.g. `dioxus-ui`) can register `window.__vcNetsim` at
// app startup — before the first meeting join — so the e2e harness can
// arm impairment pre-join and toggle it mid-call.
#[cfg(feature = "netsim")]
pub use netsim_control::install_window_hook as install_netsim_window_hook;
