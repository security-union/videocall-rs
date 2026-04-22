// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus context providers.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use dioxus_sdk_storage::{LocalStorage, StorageBacking};
use videocall_client::VideoCallClient;

/// Wrapper for the display name signal used as context.
#[derive(Clone, Copy)]
pub struct DisplayNameCtx(pub Signal<Option<String>>);

/// Local user's audio level signal, provided as context so that child
/// components (e.g. Host) can subscribe to audio-level updates without
/// forcing the parent AttendantsComponent to re-render.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct LocalAudioLevelCtx(pub Signal<f32>);

/// Glow color choices for the appearance customization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlowColor {
    White,
    Cyan,
    Magenta,
    Plum,
    MintGreen,
    Custom { r: u8, g: u8, b: u8 },
}

impl GlowColor {
    pub fn to_hex(self) -> String {
        match self {
            GlowColor::White => "#FFFFFF".to_string(),
            GlowColor::Cyan => "#0CAFFF".to_string(),
            GlowColor::Magenta => "#FF00BF".to_string(),
            GlowColor::Plum => "#DDA0DD".to_string(),
            GlowColor::MintGreen => "#5bcf9f".to_string(),
            GlowColor::Custom { r, g, b } => format!("#{r:02X}{g:02X}{b:02X}"),
        }
    }

    pub fn to_rgb(self) -> (u8, u8, u8) {
        match self {
            GlowColor::White => (255, 255, 255),
            GlowColor::Cyan => (12, 175, 255),
            GlowColor::Magenta => (255, 0, 191),
            GlowColor::Plum => (221, 160, 221),
            GlowColor::MintGreen => (91, 207, 159),
            GlowColor::Custom { r, g, b } => (r, g, b),
        }
    }

    pub fn label(self) -> String {
        match self {
            GlowColor::White => "White".to_string(),
            GlowColor::Cyan => "Cyan".to_string(),
            GlowColor::Magenta => "Magenta".to_string(),
            GlowColor::Plum => "Plum".to_string(),
            GlowColor::MintGreen => "Mint Green".to_string(),
            GlowColor::Custom { r, g, b } => format!("#{r:02X}{g:02X}{b:02X}"),
        }
    }

    fn from_storage(value: &str) -> Option<Self> {
        match value {
            "white" => Some(GlowColor::White),
            "cyan" => Some(GlowColor::Cyan),
            "magenta" => Some(GlowColor::Magenta),
            "plum" => Some(GlowColor::Plum),
            "mint-green" => Some(GlowColor::MintGreen),
            other if other.starts_with("custom:") => {
                let hex = &other[7..];
                if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                    Some(GlowColor::Custom { r, g, b })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn to_storage(self) -> String {
        match self {
            GlowColor::White => "white".to_string(),
            GlowColor::Cyan => "cyan".to_string(),
            GlowColor::Magenta => "magenta".to_string(),
            GlowColor::Plum => "plum".to_string(),
            GlowColor::MintGreen => "mint-green".to_string(),
            GlowColor::Custom { r, g, b } => format!("custom:{r:02x}{g:02x}{b:02x}"),
        }
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() == 7 && hex.starts_with('#') && hex[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
            let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
            let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
            let presets = [
                GlowColor::White,
                GlowColor::Cyan,
                GlowColor::Magenta,
                GlowColor::Plum,
                GlowColor::MintGreen,
            ];
            for preset in presets {
                if preset.to_rgb() == (r, g, b) {
                    return Some(preset);
                }
            }
            Some(GlowColor::Custom { r, g, b })
        } else {
            None
        }
    }
}

/// Local appearance settings for the speaking glow effect.
/// These settings are viewer-specific and never broadcast to other participants.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AppearanceSettings {
    pub glow_enabled: bool,
    pub glow_color: GlowColor,
    pub glow_brightness: f32,     // 0.0–1.0 scale factor
    pub inner_glow_strength: f32, // 0.0–1.0 scale factor
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        AppearanceSettings {
            glow_enabled: true,
            glow_color: GlowColor::MintGreen,
            glow_brightness: 1.0,
            inner_glow_strength: 1.0,
        }
    }
}

/// Appearance settings context for sharing custom glow preferences across the component tree.
#[derive(Clone, Copy)]
pub struct AppearanceSettingsCtx(pub Signal<AppearanceSettings>);

const APPEARANCE_GLOW_ENABLED_STORAGE_KEY: &str = "vc_appearance_glow_enabled";
const APPEARANCE_COLOR_STORAGE_KEY: &str = "vc_appearance_glow_color";
const APPEARANCE_BRIGHTNESS_STORAGE_KEY: &str = "vc_appearance_glow_brightness";
const APPEARANCE_INNER_STORAGE_KEY: &str = "vc_appearance_inner_glow_strength";
const CUSTOM_COLORS_STORAGE_KEY: &str = "vc_appearance_custom_colors";

pub const MAX_CUSTOM_COLORS: usize = 10;

/// Load local-only appearance settings from storage.
///
/// Returns defaults for any missing or invalid values.
pub fn load_appearance_settings_from_storage() -> AppearanceSettings {
    let mut settings = AppearanceSettings::default();

    if let Some(value) =
        LocalStorage::get::<String>(&APPEARANCE_GLOW_ENABLED_STORAGE_KEY.to_string())
    {
        settings.glow_enabled = value != "false";
    }

    if let Some(color) = LocalStorage::get::<String>(&APPEARANCE_COLOR_STORAGE_KEY.to_string()) {
        if let Some(parsed) = GlowColor::from_storage(&color) {
            settings.glow_color = parsed;
        }
    }

    if let Some(value) = LocalStorage::get::<f32>(&APPEARANCE_BRIGHTNESS_STORAGE_KEY.to_string()) {
        settings.glow_brightness = value.clamp(0.0, 1.0);
    }

    if let Some(value) = LocalStorage::get::<f32>(&APPEARANCE_INNER_STORAGE_KEY.to_string()) {
        settings.inner_glow_strength = value.clamp(0.0, 1.0);
    }

    settings
}

/// Save local-only appearance settings to storage.
pub fn save_appearance_settings_to_storage(settings: &AppearanceSettings) {
    LocalStorage::set(
        APPEARANCE_GLOW_ENABLED_STORAGE_KEY.to_string(),
        &settings.glow_enabled.to_string(),
    );
    LocalStorage::set(
        APPEARANCE_COLOR_STORAGE_KEY.to_string(),
        &settings.glow_color.to_storage().to_string(),
    );
    LocalStorage::set(
        APPEARANCE_BRIGHTNESS_STORAGE_KEY.to_string(),
        &settings.glow_brightness.clamp(0.0, 1.0),
    );
    LocalStorage::set(
        APPEARANCE_INNER_STORAGE_KEY.to_string(),
        &settings.inner_glow_strength.clamp(0.0, 1.0),
    );
}

/// Load custom glow colors from local storage.
pub fn load_custom_colors_from_storage() -> Vec<GlowColor> {
    let Some(csv) = LocalStorage::get::<String>(&CUSTOM_COLORS_STORAGE_KEY.to_string()) else {
        return Vec::new();
    };
    csv.split(',')
        .filter_map(|hex| {
            let hex = hex.trim();
            if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(GlowColor::Custom { r, g, b })
            } else {
                None
            }
        })
        .take(MAX_CUSTOM_COLORS)
        .collect()
}

/// Save custom glow colors to local storage.
pub fn save_custom_colors_to_storage(colors: &[GlowColor]) {
    let csv: String = colors
        .iter()
        .filter_map(|c| match c {
            GlowColor::Custom { r, g, b } => Some(format!("{r:02x}{g:02x}{b:02x}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(",");
    LocalStorage::set(CUSTOM_COLORS_STORAGE_KEY.to_string(), &csv);
}

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

/// Shared map of per-peer signal histories, provided as a Dioxus context so
/// histories survive `PeerTile` component remounts (e.g., grid -> split layout
/// when a peer starts screen sharing).
///
/// Values are `Rc<RefCell<…>>` rather than `Signal<…>` because Dioxus Signals
/// are owned by the component scope that creates them.  When a `PeerTile` is
/// destroyed (e.g. layout switch) its Signals are dropped, but the map outlives
/// that scope.  `Rc<RefCell<…>>` is scope-independent and lives as long as the
/// map holds a reference.
pub type PeerSignalHistoryMap = Signal<
    std::collections::HashMap<
        String,
        Rc<RefCell<crate::components::signal_quality::PeerSignalHistory>>,
    >,
>;

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

/// Handle a transport preference change from transport selection controls.
///
/// Shows a confirmation dialog. If the user confirms, saves the preference and
/// reloads the page. If cancelled, attempts to reset a native `<select>`
/// control (when present) back to the current value so it doesn't appear stale.
///
/// Custom controls (like the settings modal glass dropdown) are state-driven and
/// naturally re-render with the current value when the user cancels.
pub fn confirm_transport_change(new_value: &str, current: TransportPreference, select_id: &str) {
    use wasm_bindgen::JsCast;

    let pref = new_value.parse::<TransportPreference>().unwrap_or_default();
    if pref == current {
        return;
    }
    let confirmed = web_sys::window()
        .and_then(|w| {
            w.confirm_with_message(
                "Changing the transport protocol will reload the page \
                 and disconnect the current call. Continue?",
            )
            .ok()
        })
        .unwrap_or(false);
    if confirmed {
        save_transport_preference(pref);
        if let Some(w) = web_sys::window() {
            let _ = w.location().reload();
        }
    } else if let Some(select) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(select_id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlSelectElement>().ok())
    {
        select.set_value(&current.to_string());
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (re-exported from shared crate)
// ---------------------------------------------------------------------------

pub use videocall_types::validation::{
    email_to_display_name, is_guid_like, is_valid_meeting_id, validate_display_name,
    DISPLAY_NAME_MAX_LEN,
};
