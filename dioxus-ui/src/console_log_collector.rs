// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin wasm_bindgen bridge to the JS console log collector
//! (`scripts/console-log-collector.js`).
//!
//! The JS interceptor does all the heavy lifting (buffering, uploading,
//! scrubbing). This module just exposes two helpers so Rust code can call
//! `setContext` and `flush` on `window.__consoleLogCollector`.

use std::cell::RefCell;

use videocall_client::capability::{run_capability_benchmark_now, DEFAULT_BUDGET_MS};
use wasm_bindgen::prelude::*;

thread_local! {
    static CONTEXT_MEETING: RefCell<Option<String>> = const { RefCell::new(None) };
}

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
  export function set_console_log_auth_token(token) {
    // Hand the collector the current room_token so its uploads can carry
    // `Authorization: Bearer <token>`. Called on join and again on every
    // token refresh so long calls stay authenticated.
    if (window.__consoleLogCollector && window.__consoleLogCollector.setAuthToken) {
      window.__consoleLogCollector.setAuthToken(token);
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
    /// Set (or update) the room_token the collector attaches to uploads as
    /// `Authorization: Bearer <token>`. The token is a credential — never
    /// log its value. No-op if the collector is the disabled stub.
    pub fn set_console_log_auth_token(token: &str);
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
    CONTEXT_MEETING.with(|m| *m.borrow_mut() = Some(meeting_id.to_string()));
    set_console_log_context_with_capability(meeting_id, user_id, display_name, score_u32);
}

fn token_applies_to_context(meeting_id: &str) -> bool {
    CONTEXT_MEETING.with(|m| m.borrow().as_deref().is_none_or(|cur| cur == meeting_id))
}

/// [`set_console_log_auth_token`] for a token refreshed for `meeting_id`;
/// ignored once the collector's context is another meeting.
pub fn set_console_log_auth_token_for(meeting_id: &str, token: &str) {
    if token_applies_to_context(meeting_id) {
        set_console_log_auth_token(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refreshed_token_applies_only_to_the_context_meeting() {
        CONTEXT_MEETING.with(|m| *m.borrow_mut() = None);
        assert!(token_applies_to_context("meeting-a"));
        CONTEXT_MEETING.with(|m| *m.borrow_mut() = Some("meeting-b".to_string()));
        assert!(token_applies_to_context("meeting-b"));
        assert!(!token_applies_to_context("meeting-a"));
    }
}
