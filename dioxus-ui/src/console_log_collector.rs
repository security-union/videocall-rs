// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin wasm_bindgen bridge to the JS console log collector
//! (`scripts/console-log-collector.js`).
//!
//! The JS interceptor does all the heavy lifting (buffering, uploading,
//! scrubbing). This module just exposes two helpers so Rust code can call
//! `setContext` and `flush` on `window.__consoleLogCollector`.

use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = "
  export function set_console_log_context(meeting_id, user_id, display_name) {
    if (window.__consoleLogCollector) {
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
    pub fn set_console_log_context(meeting_id: &str, user_id: &str, display_name: &str);
    pub fn flush_console_logs();
}
