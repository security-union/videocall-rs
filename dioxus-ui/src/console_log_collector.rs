// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin wasm_bindgen bridge to the JS console log collector
//! (`scripts/console-log-collector.js`).
//!
//! The JS interceptor does all the heavy lifting (buffering, uploading,
//! scrubbing). This module just exposes two helpers so Rust code can call
//! `setContext` and `flush` on `window.__consoleLogCollector`.

use videocall_client::capability::{run_capability_benchmark_now, DEFAULT_BUDGET_MS};
use wasm_bindgen::prelude::*;

/// Attach the per-page capability score (Phase 8a / TELEM-2) to the JS
/// preamble before forwarding the rest of the context. Running the
/// benchmark inside Rust keeps the implementation testable and the JS
/// glue thin.
#[wasm_bindgen(inline_js = "
  export function set_console_log_context_with_capability(meeting_id, user_id, display_name, capability_score) {
    if (window.__consoleLogCollector) {
      // Stash the score in a stable global so the preamble writer can
      // read it. The console log collector reads this lazily inside
      // `writePreamble`; we set it before calling `setContext` so the
      // very first preamble line includes the value.
      window.__videocall_capability_score = capability_score;
      window.__consoleLogCollector.setContext(meeting_id, user_id, display_name);
    }
  }
  export function flush_console_logs() {
    if (window.__consoleLogCollector) {
      window.__consoleLogCollector.flush();
    }
  }
")]
extern "C" {
    fn set_console_log_context_with_capability(
        meeting_id: &str,
        user_id: &str,
        display_name: &str,
        capability_score: u32,
    );
    pub fn flush_console_logs();
}

/// Set the console-log-collector context and write the preamble line.
///
/// Wraps the JS shim so callers don't have to know about the capability
/// benchmark — we run it here, clamp to `u32`, and pass it along. The
/// benchmark runs for ~100 ms wall time; callers should expect this to
/// block briefly on join.
pub fn set_console_log_context(meeting_id: &str, user_id: &str, display_name: &str) {
    let score = run_capability_benchmark_now(DEFAULT_BUDGET_MS);
    let score_u32 = if score > u32::MAX as u64 {
        u32::MAX
    } else {
        score as u32
    };
    set_console_log_context_with_capability(meeting_id, user_id, display_name, score_u32);
}
