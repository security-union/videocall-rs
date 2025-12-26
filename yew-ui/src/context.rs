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

/// Holds meeting timing information for components that need it.
/// Updated by AttendantsComponent when connection events occur.
#[derive(Clone, PartialEq, Default)]
pub struct MeetingTime {
    /// Unix timestamp (ms) when the current user joined the call
    pub call_start_time: Option<f64>,
    /// Unix timestamp (ms) when the meeting started (from server)
    pub meeting_start_time: Option<f64>,
}

/// Context type for meeting time - read-only access to timing info
pub type MeetingTimeCtx = MeetingTime;

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
