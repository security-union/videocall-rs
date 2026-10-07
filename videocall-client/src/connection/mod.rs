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
#[cfg(test)]
mod log_capture {
    use std::cell::RefCell;

    struct CaptureLogger;

    type Captured = (&'static str, Vec<(log::Level, String)>);

    thread_local! {
        static CAPTURED_LOGS: RefCell<Option<Captured>> = const { RefCell::new(None) };
    }

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            CAPTURED_LOGS.with(|c| {
                if let Some((target, lines)) = c.borrow_mut().as_mut() {
                    if record.target() == *target {
                        lines.push((record.level(), record.args().to_string()));
                    }
                }
            });
        }

        fn flush(&self) {}
    }

    pub(crate) fn capture_logs(
        target: &'static str,
        body: impl FnOnce(),
    ) -> Vec<(log::Level, String)> {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&CaptureLogger).expect("no other logger in this test binary")
        });
        let _guard = crate::test_serial::lock_log_max_level();
        let previous = log::max_level();
        log::set_max_level(log::LevelFilter::Info);
        CAPTURED_LOGS.with(|c| *c.borrow_mut() = Some((target, Vec::new())));
        body();
        log::set_max_level(previous);
        CAPTURED_LOGS.with(|c| {
            c.borrow_mut()
                .take()
                .map(|(_, lines)| lines)
                .unwrap_or_default()
        })
    }
}
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
// The per-transport connection-loss readers (#509 item #4) are a public
// observability surface (perf panel / the documented telemetry follow-up). No
// in-crate consumer reads them yet — they are split client-side for local
// debuggability without a protobuf change — so the re-export is intentionally
// allowed to be unused, matching the `ReconnectionPhase` re-export above.
pub use connection_manager::{
    connection_handshake_failures, connection_session_drops, reelection_aborted_total,
    reelection_failed_total, reelection_preserved_total, reelection_proceeded_total,
    ConnectionManagerOptions, ConnectionState, SessionIdHistory,
};
#[allow(unused_imports)]
pub use connection_manager::{
    connection_handshake_failures_ws, connection_handshake_failures_wt,
    connection_session_drops_ws, connection_session_drops_wt,
};
pub use webmedia::{ConnectOptions, MediaStreamKey};
// #2746: test-only, to pin this whitelist against the peer-creation gate.
#[cfg(test)]
pub(crate) use connection_manager::should_filter_self_packet;

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) use connection_controller::host_seam::take_adopt_wt_spare_worker;

// Issue #1080: the runtime netsim control-surface installer, re-exported
// so the UI crate (e.g. `dioxus-ui`) can register `window.__vcNetsim` at
// app startup — before the first meeting join — so the e2e harness can
// arm impairment pre-join and toggle it mid-call.
#[cfg(feature = "netsim")]
pub use netsim_control::install_window_hook as install_netsim_window_hook;
#[cfg(all(test, feature = "netsim"))]
pub(crate) use netsim_control::{
    force_camera_keyframe_for_netsim, set_ws_buffered_override_for_netsim,
};
#[cfg(feature = "netsim")]
pub(crate) use netsim_control::{
    register_camera_force_keyframe_for_netsim, ws_buffered_override_for_netsim,
};
