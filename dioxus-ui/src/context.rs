// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus's context system.

use dioxus::prelude::*;
use videocall_client::VideoCallClient;

/// Type alias used throughout the app when accessing the username context.
///
/// `Signal<Option<String>>` allows both read-only access and mutation.
pub type UsernameCtx = Signal<Option<String>>;

/// VideoCallClient context for sharing the client instance across components.
///
/// This eliminates props drilling and provides clean access to the client
/// from any component in the tree.
pub type VideoCallClientCtx = VideoCallClient;

// -----------------------------------------------------------------------------
// Meeting Time Context
// -----------------------------------------------------------------------------

/// Holds meeting timing information shared via Dioxus context.
///
/// # Lifecycle
/// - Created with `Default::default()` (both fields `None`)
/// - `call_start_time` is set when WebSocket/WebTransport connection succeeds
/// - `meeting_start_time` is set when `MEETING_STARTED` packet is received from server
///
/// # Usage
/// Components access this via `use_context::<Signal<MeetingTime>>()`. If context is
/// missing, `unwrap_or_default()` returns empty values and timers show "--:--".
#[derive(Clone, PartialEq, Default)]
pub struct MeetingTime {
    /// Unix timestamp (ms) when the current user joined the call.
    /// Set on successful connection. `None` before connection.
    pub call_start_time: Option<f64>,

    /// Unix timestamp (ms) when the meeting started (from server).
    /// Set when `MEETING_STARTED` packet is received. `None` if not yet received.
    pub meeting_start_time: Option<f64>,
}

/// Context type for meeting time - read-only access to timing info.
pub type MeetingTimeCtx = Signal<MeetingTime>;

// -----------------------------------------------------------------------------
// Meeting Host Context
// -----------------------------------------------------------------------------

/// Holds meeting host information shared via Dioxus context.
///
/// Used to identify the meeting owner/host and display appropriate UI indicators.
#[derive(Clone, PartialEq, Default)]
pub struct MeetingHost {
    /// Email/ID of the meeting host. `None` if not yet known.
    pub host_email: Option<String>,
}

impl MeetingHost {
    /// Check if the given email is the meeting host
    #[allow(dead_code)]
    pub fn is_host(&self, email: &str) -> bool {
        self.host_email.as_deref() == Some(email)
    }
}

/// Context type for meeting host - read-only access to host info.
#[allow(dead_code)]
pub type MeetingHostCtx = Signal<MeetingHost>;

// -----------------------------------------------------------------------------
// Local-storage helpers
// -----------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_username";

/// Read the username from `window.localStorage` (if present).
pub fn load_username_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

/// Persist the username to `localStorage` so that it survives page reloads.
pub fn save_username_to_storage(username: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, username);
    }
}

// -----------------------------------------------------------------------------
// Validation helpers
// -----------------------------------------------------------------------------

use once_cell::sync::Lazy;

static USERNAME_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^[A-Za-z0-9_]+$").unwrap());

/// Returns `true` iff the supplied username is non-empty and matches the
/// allowed pattern.
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty() && USERNAME_RE.is_match(name)
}
