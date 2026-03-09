// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Yew's `ContextProvider`.

use videocall_client::VideoCallClient;
use yew::prelude::*;

/// Type alias used throughout the app when accessing the display name context.
///
/// `UseStateHandle<Option<String>>` allows both read-only access (via
/// deref) and mutation by calling `.set(Some("new_name".into()))`.
pub type DisplayNameCtx = UseStateHandle<Option<String>>;

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
    /// User ID of the meeting host. `None` if not yet known.
    pub host_user_id: Option<String>,
}

impl MeetingHost {
    /// Check if the given user_id is the meeting host
    #[allow(dead_code)]
    pub fn is_host(&self, user_id: &str) -> bool {
        self.host_user_id.as_deref() == Some(user_id)
    }
}

/// Context type for meeting host — User ID of the meeting host.
#[allow(dead_code)]
pub type MeetingHostCtx = MeetingHost;

// -----------------------------------------------------------------------------
// Local-storage helpers
// -----------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_display_name";

/// Read the display name from `window.localStorage` (if present).
pub fn load_display_name_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

/// Persist the display name to `localStorage` so that it survives page reloads.
pub fn save_display_name_to_storage(display_name: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, display_name);
    }
}

/// Remove the display name from `localStorage` entirely (e.g. on logout).
pub fn clear_display_name_from_storage() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(STORAGE_KEY);
    }
}

// ---------------------------------------------------------------------------
// Persistent local user ID
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const USER_ID_STORAGE_KEY: &str = "vc_user_id";

/// Get or create a persistent local user ID.
///
/// When OAuth is enabled the meeting API provides the `user_id` from the
/// identity service.  When OAuth is disabled we generate a unique identifier
/// and persist it in `localStorage` so the same browser always presents the
/// same identity.
#[allow(dead_code)]
pub fn get_or_create_local_user_id() -> String {
    let window = web_sys::window().expect("no window");
    if let Some(storage) = window.local_storage().ok().flatten() {
        if let Ok(Some(id)) = storage.get_item(USER_ID_STORAGE_KEY) {
            if !id.is_empty() {
                return id;
            }
        }
        let id = generate_uuid();
        let _ = storage.set_item(USER_ID_STORAGE_KEY, &id);
        id
    } else {
        // localStorage unavailable — generate an ephemeral ID.
        generate_uuid()
    }
}

/// Generate a unique identifier from the current timestamp and a random
/// component.  We intentionally avoid pulling in the `uuid` crate to keep
/// the WASM binary small.
fn generate_uuid() -> String {
    use js_sys::Math;
    let millis = web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let rand = (Math::random() * 1_000_000_000.0) as u64;
    format!("{millis:x}-{rand:x}")
}

// ---------------------------------------------------------------------------
// Legacy storage migration
// ---------------------------------------------------------------------------

/// Migrate old `localStorage` keys to their current names (one-time).
///
/// Earlier builds stored the display name under `vc_username`.  This helper
/// copies the value to `vc_display_name` and removes the old key so that
/// returning users keep their name without manual re-entry.
pub fn migrate_legacy_storage() {
    let window = web_sys::window().expect("no window");
    if let Some(storage) = window.local_storage().ok().flatten() {
        // Migrate vc_username -> vc_display_name
        if storage.get_item(STORAGE_KEY).ok().flatten().is_none() {
            if let Ok(Some(old_val)) = storage.get_item("vc_username") {
                let _ = storage.set_item(STORAGE_KEY, &old_val);
                let _ = storage.remove_item("vc_username");
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Validation helpers (re-exported from shared crate)
// -----------------------------------------------------------------------------

pub use videocall_types::validation::{
    email_to_display_name, is_valid_meeting_id, validate_display_name, DISPLAY_NAME_MAX_LEN,
};
