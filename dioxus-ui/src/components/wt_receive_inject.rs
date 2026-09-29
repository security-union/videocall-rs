// SPDX-License-Identifier: MIT OR Apache-2.0

//! Gated registration of the #2728 WebTransport receive-path read-back.

/// Register the read-back, gated on `MOCK_PEERS_ENABLED`.
#[cfg(target_arch = "wasm32")]
pub fn register_wt_receive_stats_hook() {
    if !crate::constants::mock_peers_enabled() {
        return;
    }
    videocall_client::wt_receive_inject::register_wt_receive_stats_hook();
}

/// Native stub: no `window`, nothing to register.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_wt_receive_stats_hook() {}
