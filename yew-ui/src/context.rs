// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Yew's `ContextProvider`.

use videocall_client::VideoCallClient;
use yew::prelude::*;

/// Type alias used throughout the app when accessing the username context.
///
/// `UseStateHandle<Option<String>>` allows both read-only access (via
/// deref) and mutation by calling `.set(Some("new_name".into()))`.
pub type UsernameCtx = UseStateHandle<Option<String>>;

/// VideoCallClient context for sharing the client instance across components.
///
/// This eliminates props drilling and provides clean access to the client
/// from any component in the tree.
pub type VideoCallClientCtx = VideoCallClient;

// -----------------------------------------------------------------------------
// Meeting Time Context
// -----------------------------------------------------------------------------

/// Holds meeting timing information shared via Yew context.
///
/// # Lifecycle
/// - Created with `Default::default()` (both fields `None`)
/// - `call_start_time` is set when WebSocket/WebTransport connection succeeds
/// - `meeting_start_time` is set when `MEETING_STARTED` packet is received from server
///
/// # Usage
/// Components access this via `use_context::<MeetingTimeCtx>()`. If context is
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
pub type MeetingTimeCtx = MeetingTime;

// -----------------------------------------------------------------------------
// Meeting Host Context
// -----------------------------------------------------------------------------

/// Holds meeting host information shared via Yew context.
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
pub type MeetingHostCtx = MeetingHost;

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

/// Remove the username from `localStorage` entirely (e.g. on logout).
pub fn clear_username_from_storage() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(STORAGE_KEY);
    }
}

// -----------------------------------------------------------------------------
// Validation helpers (re-exported from shared crate)
// -----------------------------------------------------------------------------

#[allow(unused_imports)] // normalize_spaces is used in integration tests
pub use videocall_types::validation::{
    email_to_display_name, normalize_spaces, validate_display_name, DISPLAY_NAME_MAX_LEN,
};

/// Backward-compatible alias -- prefer `is_valid_meeting_id` for new code.
pub use videocall_types::validation::is_valid_meeting_id as is_valid_username;
