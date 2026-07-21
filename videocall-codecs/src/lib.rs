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

//! A high-fidelity, cross-platform video decoder jitter buffer implementation in Rust.

pub mod decoder;
pub mod encoder;
pub mod frame;
pub mod jitter_buffer;
pub mod jitter_estimator;
pub mod messages;
pub mod vp9;

// UniFFI Swift/Kotlin bindings for the pure-Rust VP9 codec. Native-only (the
// `uniffi` CLI machinery does not build for wasm32), gated behind the `uniffi`
// feature so nothing else is affected.
#[cfg(all(feature = "uniffi", not(target_arch = "wasm32")))]
uniffi::setup_scaffolding!();
#[cfg(all(feature = "uniffi", not(target_arch = "wasm32")))]
mod ffi;

/// Deterministic synthetic sources, PSNR metrics, an IVF writer, and (behind
/// `libvpx`) a synchronous libvpx decode oracle. Used by the VP9 encoder's
/// TDD harness. Enabled with the `test-utils` feature.
#[cfg(feature = "test-utils")]
pub mod testing;

// Diagnostics helper to publish video metrics via the shared event bus.
#[cfg(feature = "wasm")]
pub mod video_diagnostics {
    use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};

    /// Publish video stats to the global diagnostics stream. `stream_id` should be
    /// in the format "from_peer->to_peer" to align with health reporting expectations.
    pub fn report_video_stats(stream_id: String, fps: Option<f64>, frames_buffered: Option<u64>) {
        let mut metrics = Vec::new();
        if let Some(f) = fps {
            metrics.push(metric!("fps_received", f));
        }
        if let Some(b) = frames_buffered {
            metrics.push(metric!("frames_buffered", b));
        }

        if metrics.is_empty() {
            return;
        }

        let event = DiagEvent {
            subsystem: "video",
            stream_id: Some(stream_id),
            ts_ms: now_ms(),
            metrics,
        };
        // Best-effort broadcast; ignore backpressure errors
        let _ = global_sender().try_broadcast(event);
    }
}
