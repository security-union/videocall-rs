// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus context providers.

use dioxus::prelude::*;
use videocall_client::VideoCallClient;

/// Wrapper for the display name signal used as context.
#[derive(Clone, Copy)]
pub struct DisplayNameCtx(pub Signal<Option<String>>);

/// Local user's audio level signal, provided as context so that child
/// components (e.g. Host) can subscribe to audio-level updates without
/// forcing the parent AttendantsComponent to re-render.
#[derive(Clone, Copy)]
pub struct LocalAudioLevelCtx(pub Signal<f32>);

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

pub fn load_display_name_from_storage() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
}

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

const USER_ID_STORAGE_KEY: &str = "vc_user_id";

/// Get or create a persistent local user ID.
///
/// When OAuth is enabled the meeting API provides the `user_id` from the
/// identity service.  When OAuth is disabled we generate a unique identifier
/// and persist it in `localStorage` so the same browser always presents the
/// same identity.
pub fn get_or_create_local_user_id() -> String {
    let window = web_sys::window().expect("no window");
    if let Some(storage) = window.local_storage().ok().flatten() {
        if let Ok(Some(id)) = storage.get_item(USER_ID_STORAGE_KEY) {
            if !id.is_empty() {
                return id;
            }
        }
        let id = generate_local_id();
        let _ = storage.set_item(USER_ID_STORAGE_KEY, &id);
        id
    } else {
        // localStorage unavailable — generate an ephemeral ID.
        generate_local_id()
    }
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

// ---------------------------------------------------------------------------
// Transport preference
// ---------------------------------------------------------------------------

/// User-facing transport protocol preference.
///
/// Stored in `localStorage` under `vc_transport_preference` and read at
/// connection time to override the server-provided WebTransport flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TransportPreference {
    /// Honour the server-side `webTransportEnabled` flag (default behaviour).
    #[default]
    Auto,
    /// Force WebTransport — WebSocket URLs are cleared.
    WebTransportOnly,
    /// Force WebSocket — WebTransport is disabled.
    WebSocketOnly,
}

impl std::fmt::Display for TransportPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TransportPreference::Auto => "auto",
            TransportPreference::WebTransportOnly => "webtransport",
            TransportPreference::WebSocketOnly => "websocket",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for TransportPreference {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(TransportPreference::Auto),
            "webtransport" => Ok(TransportPreference::WebTransportOnly),
            "websocket" => Ok(TransportPreference::WebSocketOnly),
            _ => Err(()),
        }
    }
}

/// Context wrapper for the transport preference signal.
#[derive(Clone, Copy)]
pub struct TransportPreferenceCtx(pub Signal<TransportPreference>);

const TRANSPORT_PREF_KEY: &str = "vc_transport_preference";

/// Load the persisted transport preference from `localStorage`.
pub fn load_transport_preference() -> TransportPreference {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(TRANSPORT_PREF_KEY).ok().flatten())
        .and_then(|val| val.parse::<TransportPreference>().ok())
        .unwrap_or_default()
}

/// Persist the transport preference to `localStorage`.
pub fn save_transport_preference(pref: TransportPreference) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(TRANSPORT_PREF_KEY, &pref.to_string());
    }
}

/// Resolve effective transport configuration from the user's preference and
/// the server-provided WebTransport flag.
///
/// Returns `(enable_webtransport, websocket_urls, webtransport_urls)`.
pub fn resolve_transport_config(
    pref: TransportPreference,
    server_wt_enabled: bool,
    ws_urls: Vec<String>,
    wt_urls: Vec<String>,
) -> (bool, Vec<String>, Vec<String>) {
    match pref {
        TransportPreference::Auto => (server_wt_enabled, ws_urls, wt_urls),
        TransportPreference::WebTransportOnly => (true, vec![], wt_urls),
        TransportPreference::WebSocketOnly => (false, ws_urls, vec![]),
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (re-exported from shared crate)
// ---------------------------------------------------------------------------

pub use videocall_types::validation::{
    email_to_display_name, is_valid_meeting_id, validate_display_name, DISPLAY_NAME_MAX_LEN,
};
