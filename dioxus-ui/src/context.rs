// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus context providers.

use dioxus::prelude::*;
use dioxus_sdk_storage::{LocalStorage, StorageBacking};
use videocall_client::VideoCallClient;

/// Wrapper for the display name signal used as context.
#[derive(Clone, Copy)]
pub struct DisplayNameCtx(pub Signal<Option<String>>);

/// VideoCallClient context for sharing the client instance across components.
pub type VideoCallClientCtx = VideoCallClient;

/// Holds meeting timing information shared via context.
#[derive(Clone, PartialEq, Default)]
pub struct MeetingTime {
    pub call_start_time: Option<f64>,
    pub meeting_start_time: Option<f64>,
}

pub type MeetingTimeCtx = Signal<MeetingTime>;

/// Per-peer media state tracked by the shared diagnostics subscriber.
#[derive(Clone, Default, PartialEq)]
pub struct PeerMediaState {
    pub audio_enabled: bool,
    pub video_enabled: bool,
    pub screen_enabled: bool,
}

/// Shared map of per-peer media state signals, provided as a Dioxus context.
///
/// A single async task subscribes to the diagnostics broadcast channel and
/// updates per-peer signals.  Each `PeerTile` reads only its own
/// `Signal<PeerMediaState>`, so a state change for peer A does not cause
/// peer B's tile to re-render.
pub type PeerStatusMap = Signal<std::collections::HashMap<String, Signal<PeerMediaState>>>;

/// Holds meeting host information shared via context.
#[derive(Clone, PartialEq, Default)]
#[allow(dead_code)]
pub struct MeetingHost {
    pub host_user_id: Option<String>,
}

impl MeetingHost {
    #[allow(dead_code)]
    pub fn is_host(&self, user_id: &str) -> bool {
        self.host_user_id.as_deref() == Some(user_id)
    }
}

#[allow(dead_code)]
pub type MeetingHostCtx = Signal<MeetingHost>;

// ---------------------------------------------------------------------------
// Local-storage helpers
// ---------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_display_name";

/// Load the persisted display name from local storage.
///
/// Uses [`dioxus_sdk_storage::LocalStorage`] which maps to the browser's
/// `localStorage` on web and the file system on native platforms.  Returns
/// `None` when no name has been saved yet, or when the stored value is empty.
pub fn load_display_name_from_storage() -> Option<String> {
    LocalStorage::get::<Option<String>>(&STORAGE_KEY.to_string())
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Persist the display name to local storage.
pub fn save_display_name_to_storage(display_name: &str) {
    LocalStorage::set(STORAGE_KEY.to_string(), &Some(display_name.to_string()));
}

/// Remove the display name from local storage entirely (e.g. on logout).
pub fn clear_display_name_from_storage() {
    LocalStorage::set(STORAGE_KEY.to_string(), &None::<String>);
}

// ---------------------------------------------------------------------------
// Persistent local user ID
// ---------------------------------------------------------------------------

const USER_ID_STORAGE_KEY: &str = "vc_user_id";

/// Get or create a persistent local user ID.
///
/// When OAuth is enabled the meeting API provides the `user_id` from the
/// identity service.  When OAuth is disabled we generate a unique identifier
/// and persist it via [`LocalStorage`] so the same browser/device always
/// presents the same identity.
pub fn get_or_create_local_user_id() -> String {
    if let Some(id) =
        LocalStorage::get::<String>(&USER_ID_STORAGE_KEY.to_string()).filter(|s| !s.is_empty())
    {
        return id;
    }
    let id = generate_local_id();
    LocalStorage::set(USER_ID_STORAGE_KEY.to_string(), &id);
    id
}

/// Generate a unique identifier from the current timestamp and a random
/// component.  We intentionally avoid pulling in the `uuid` crate to keep
/// the WASM binary small.
fn generate_local_id() -> String {
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

/// One-time migration from the old plain-string `localStorage` format to the
/// CBOR+zlib encoding used by [`dioxus_sdk_storage::LocalStorage`].
///
/// Earlier builds stored `vc_display_name` (and `vc_username` in very old
/// releases) as raw uncompressed strings directly in the browser's
/// `localStorage`.  The new storage backend uses CBOR+zlib serialisation,
/// which is unreadable by `load_display_name_from_storage` when the stored
/// bytes are in the old format.  This function detects that situation on the
/// first startup after an upgrade and re-writes the value in the new format
/// so returning users keep their saved display name without re-entry.
///
/// Must be called at app startup **before** the Dioxus component tree mounts,
/// which is why it lives in `main.rs` before `dioxus::launch`.  It is a
/// no-op when the new-format value already exists or on non-web platforms
/// (where there is no legacy plain-string data).
///
/// **Removal:** once all production deployments have been running the new
/// code long enough that stale plain-string values are gone (typically a
/// few weeks), this function and the `web-sys` `Storage` feature it relies
/// on can be dropped.
pub fn migrate_legacy_storage() {
    // Only needed on web where the old plain-string format was ever written.
    #[cfg(target_family = "wasm")]
    {
        // If the new CBOR format already has a value, nothing to migrate.
        //
        // Note: `load_display_name_from_storage()` returns `None` for both
        // "key absent" **and** "key present but encoded in the old plain-string
        // format" — dioxus_sdk_storage silently returns `None` on a CBOR
        // deserialisation failure.  That dual-None behaviour is exactly what
        // makes this guard correct: the early return fires only when new-format
        // data already exists, never for stale plain-string data.
        if load_display_name_from_storage().is_some() {
            return;
        }

        let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
            return;
        };

        // Try the current key, then the legacy key used in older releases.
        let value = storage
            .get_item(STORAGE_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                storage
                    .get_item("vc_username")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty())
            });

        if let Some(v) = value {
            // Re-store in the new CBOR+zlib format.
            save_display_name_to_storage(&v);
        }
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (re-exported from shared crate)
// ---------------------------------------------------------------------------

pub use videocall_types::validation::{
    email_to_display_name, is_valid_meeting_id, validate_display_name, DISPLAY_NAME_MAX_LEN,
};
