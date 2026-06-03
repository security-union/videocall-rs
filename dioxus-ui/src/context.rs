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

/// Per-tile crop state: canvas ID → is-cropped.
/// Survives re-renders caused by peer list changes so crop toggles persist.
#[derive(Clone, Copy)]
pub struct CroppedTilesCtx(pub Signal<std::collections::HashMap<String, bool>>);

/// Action bar dock position (Bottom / Left / Right).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockPosition {
    Bottom,
    Left,
    Right,
}

impl DockPosition {
    pub fn css_class(self) -> &'static str {
        match self {
            DockPosition::Bottom => "dock-bottom",
            DockPosition::Left => "dock-left",
            DockPosition::Right => "dock-right",
        }
    }

    #[allow(dead_code)]
    pub fn next(self) -> Self {
        match self {
            DockPosition::Bottom => DockPosition::Left,
            DockPosition::Left => DockPosition::Right,
            DockPosition::Right => DockPosition::Bottom,
        }
    }
}

/// Context for the action bar dock position (Bottom / Left / Right).
#[derive(Clone, Copy)]
pub struct DockPositionCtx(pub Signal<DockPosition>);

/// Context for the action bar autohide setting.
#[derive(Clone, Copy)]
pub struct AutohideCtx(pub Signal<bool>);

// ---------------------------------------------------------------------------
// Dock position & autohide persistence
// ---------------------------------------------------------------------------

const DOCK_POSITION_KEY: &str = "vc_dock_position";
const DOCK_AUTOHIDE_KEY: &str = "vc_dock_autohide";

/// Load dock position from localStorage. Defaults to Bottom.
pub fn load_dock_position() -> DockPosition {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DOCK_POSITION_KEY).ok().flatten())
        .map(|v| match v.as_str() {
            "left" => DockPosition::Left,
            "right" => DockPosition::Right,
            _ => DockPosition::Bottom,
        })
        .unwrap_or(DockPosition::Bottom)
}

/// Resolve a raw localStorage value (e.g. `Some("true")`, `Some("false")`, or
/// `None` when no preference has been persisted) into the initial autohide
/// signal value. When no preference is stored, default to `false` (always
/// visible) so first-time users see the action bar without learning the
/// dock menu first.
pub fn resolve_dock_autohide(stored: Option<&str>) -> bool {
    match stored {
        Some(v) => v != "false",
        None => false,
    }
}

/// Load dock autohide from localStorage. Defaults to `false` (no hiding)
/// when no preference has been persisted yet.
pub fn load_dock_autohide() -> bool {
    let stored = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DOCK_AUTOHIDE_KEY).ok().flatten());
    resolve_dock_autohide(stored.as_deref())
}

/// Persist dock position to localStorage.
pub fn save_dock_position(pos: DockPosition) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let val = match pos {
            DockPosition::Bottom => "bottom",
            DockPosition::Left => "left",
            DockPosition::Right => "right",
        };
        let _ = storage.set_item(DOCK_POSITION_KEY, val);
    }
}

/// Persist dock autohide to localStorage.
pub fn save_dock_autohide(enabled: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(DOCK_AUTOHIDE_KEY, if enabled { "true" } else { "false" });
    }
}

// ---------------------------------------------------------------------------
// Density mode persistence
// ---------------------------------------------------------------------------

use crate::components::density::DensityMode;

/// Context for the tile density mode.
#[derive(Clone, Copy)]
pub struct DensityModeCtx(pub Signal<DensityMode>);

const DENSITY_MODE_KEY: &str = "vc_density_mode";

/// Load density mode from localStorage. Defaults to Auto.
pub fn load_density_mode() -> DensityMode {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DENSITY_MODE_KEY).ok().flatten())
        .map(|v| match v.as_str() {
            "standard" => DensityMode::Standard,
            "dense" => DensityMode::Dense,
            "maximum" => DensityMode::Maximum,
            _ => DensityMode::Auto,
        })
        .unwrap_or(DensityMode::Auto)
}

/// Persist density mode to localStorage.
pub fn save_density_mode(mode: DensityMode) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let val = match mode {
            DensityMode::Auto => "auto",
            DensityMode::Standard => "standard",
            DensityMode::Dense => "dense",
            DensityMode::Maximum => "maximum",
        };
        let _ = storage.set_item(DENSITY_MODE_KEY, val);
    }
}

// ---------------------------------------------------------------------------
// Decode-budget override persistence
// ---------------------------------------------------------------------------

/// Manual override for the adaptive decode-budget controller.
///
/// `Auto` (the default) lets the adaptive control loop in `attendants.rs`
/// decide how many tiles to decode. `Fixed(n)` is a **hard override**: it
/// forces exactly `n` decoded tiles and bypasses the auto-loop entirely. This
/// type is purely the persisted/shared state — the bypass behavior lives in
/// the control loop (task 1a.3), not here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DecodeBudgetOverride {
    #[default]
    Auto,
    Fixed(usize),
}

/// Context for the decode-budget override.
#[derive(Clone, Copy)]
pub struct DecodeBudgetCtx(pub Signal<DecodeBudgetOverride>);

const DECODE_BUDGET_OVERRIDE_KEY: &str = "vc_decode_budget_override";

/// Parse a persisted decode-budget override string. Mirrors the density-mode
/// manual-match style: `"auto"` (or any unparseable value) yields the default
/// `Auto`; a positive integer string yields `Fixed(n)`. A stored `Fixed(0)`
/// (or any value that fails to parse as a non-zero `usize`) collapses to
/// `Auto`, since a zero-tile hard override is meaningless.
fn parse_decode_budget_override(raw: &str) -> DecodeBudgetOverride {
    match raw {
        "auto" => DecodeBudgetOverride::Auto,
        other => match other.parse::<usize>() {
            Ok(n) if n > 0 => DecodeBudgetOverride::Fixed(n),
            _ => DecodeBudgetOverride::Auto,
        },
    }
}

/// Serialize a decode-budget override to its compact storage string: `"auto"`
/// for `Auto`, or the bare integer for `Fixed(n)`.
fn serialize_decode_budget_override(value: DecodeBudgetOverride) -> String {
    match value {
        DecodeBudgetOverride::Auto => "auto".to_string(),
        DecodeBudgetOverride::Fixed(n) => n.to_string(),
    }
}

/// Load the decode-budget override from localStorage. Defaults to `Auto`.
pub fn load_decode_budget_override() -> DecodeBudgetOverride {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DECODE_BUDGET_OVERRIDE_KEY).ok().flatten())
        .map(|v| parse_decode_budget_override(&v))
        .unwrap_or_default()
}

/// Persist the decode-budget override to localStorage.
pub fn save_decode_budget_override(value: DecodeBudgetOverride) {
    // User override engaging: log the user's explicit choice (Fixed cap vs Auto
    // resume) so support can distinguish a user-chosen cap from an auto-shed.
    match value {
        DecodeBudgetOverride::Fixed(n) => {
            log::info!("DecodeBudget: override=fixed n={n} source=user_setting")
        }
        DecodeBudgetOverride::Auto => {
            log::info!("DecodeBudget: override=auto source=user_setting")
        }
    }
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(
            DECODE_BUDGET_OVERRIDE_KEY,
            &serialize_decode_budget_override(value),
        );
    }
}

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
    pub show_join_leave_notifications: bool,
    pub play_join_leave_sounds: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        AppearanceSettings {
            glow_enabled: true,
            glow_color: GlowColor::MintGreen,
            glow_brightness: 1.0,
            inner_glow_strength: 1.0,
            show_join_leave_notifications: true,
            play_join_leave_sounds: true,
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
const APPEARANCE_JOIN_LEAVE_NOTIFICATIONS_KEY: &str = "vc_appearance_join_leave_notifications";
const APPEARANCE_JOIN_LEAVE_SOUNDS_KEY: &str = "vc_appearance_join_leave_sounds";
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

    if let Some(value) =
        LocalStorage::get::<String>(&APPEARANCE_JOIN_LEAVE_NOTIFICATIONS_KEY.to_string())
    {
        settings.show_join_leave_notifications = value != "false";
    }

    if let Some(value) = LocalStorage::get::<String>(&APPEARANCE_JOIN_LEAVE_SOUNDS_KEY.to_string())
    {
        settings.play_join_leave_sounds = value != "false";
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
    LocalStorage::set(
        APPEARANCE_JOIN_LEAVE_NOTIFICATIONS_KEY.to_string(),
        &settings.show_join_leave_notifications.to_string(),
    );
    LocalStorage::set(
        APPEARANCE_JOIN_LEAVE_SOUNDS_KEY.to_string(),
        &settings.play_join_leave_sounds.to_string(),
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
        .take(MAX_CUSTOM_COLORS)
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

/// HCL bug #8 / #9: per-(peer, mode) signal-quality popup state, lifted out
/// of `PeerTile`'s per-component lifecycle so a peer leaving the meeting (or
/// a layout switch between grid / split / full-bleed) does not unmount every
/// open popup. Only the popup whose anchored peer left is dropped; all other
/// open popups survive untouched.
///
/// Bug #9 also stores the user's drag-and-drop position here so that
/// switching layouts (grid → split when a peer starts screen sharing, etc.)
/// keeps the popup pinned to wherever the user dragged it.
pub type SignalPopupStateMap = Signal<
    std::collections::HashMap<
        (String, crate::components::signal_quality::SignalMeterMode),
        crate::components::signal_quality::SignalPopupState,
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
// Pre-join device preference persistence (issue #959)
// ---------------------------------------------------------------------------
//
// The pre-join device-preview screen lets the user pick which camera, mic, and
// speaker to use and whether to start with the camera/mic on, BEFORE joining.
// We persist those choices in `localStorage` (mirroring the display-name /
// transport-preference pattern above) and restore them on the next visit. The
// in-meeting `Host` reads the stored device IDs on first device enumeration so
// the pre-join selection is the one actually used when capture starts.

const DEVICE_PREF_CAMERA_KEY: &str = "vc_prejoin_camera_id";
const DEVICE_PREF_MIC_KEY: &str = "vc_prejoin_mic_id";
const DEVICE_PREF_SPEAKER_KEY: &str = "vc_prejoin_speaker_id";
const DEVICE_PREF_CAMERA_ON_KEY: &str = "vc_prejoin_camera_on";
const DEVICE_PREF_MIC_ON_KEY: &str = "vc_prejoin_mic_on";

fn read_local_storage(key: &str) -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
        .filter(|v| !v.is_empty())
}

fn write_local_storage(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}

/// Resolve which device ID to actually use given the user's stored preference
/// and the list of currently-available device IDs.
///
/// Resolution rules (pure; no DOM access so it is host-testable):
///
/// 1. If a `stored` ID is present AND still exists in `available`, use it.
/// 2. Otherwise fall back to the first available device ID (the same
///    "default = device 0" semantics `SelectableDevices::selected()` uses).
/// 3. If `available` is empty, return `None`.
///
/// This makes us resilient to a persisted device that was unplugged between
/// visits — we never select a phantom device, we just fall back to the default.
pub fn restore_device_id(stored: Option<&str>, available: &[String]) -> Option<String> {
    if let Some(id) = stored {
        if !id.is_empty() && available.iter().any(|a| a == id) {
            return Some(id.to_string());
        }
    }
    available.first().cloned()
}

/// Resolve the initial on/off state to apply for a track when joining.
///
/// Pure decision function (host-testable). The user's stored preference only
/// takes effect when the corresponding permission was granted AND a device of
/// that kind actually exists. If permission was denied or no device is present,
/// the track must start OFF regardless of the stored preference — we never try
/// to enable capture we cannot perform.
pub fn resolve_initial_enabled(
    stored_on: bool,
    permission_granted: bool,
    has_device: bool,
) -> bool {
    stored_on && permission_granted && has_device
}

/// Feature-detect `HTMLMediaElement.prototype.setSinkId` support, given a
/// capability flag. Pulled out as a pure function so the decision logic is
/// host-testable; the actual JS feature probe lives in
/// [`html_media_set_sink_id_supported`].
///
/// Chromium-based browsers expose `setSinkId`, allowing programmatic audio
/// output (speaker) selection. Firefox and Safari do not, so the speaker
/// dropdown must be rendered read-only there with an explanatory note.
///
/// The only non-test caller is the wasm-gated [`html_media_set_sink_id_supported`],
/// so on the native build (which `cargo clippy --all` compiles) this has no
/// caller outside `#[cfg(test)]`. Suppress dead_code only there — on wasm it is
/// genuinely used, and the host test still exercises it under `cargo test`.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub fn speaker_selection_supported(set_sink_id_present: bool) -> bool {
    set_sink_id_present
}

/// Probe the live browser for `HTMLMediaElement.prototype.setSinkId` support.
///
/// Returns `false` on non-web targets or when the prototype is unreachable.
#[cfg(target_family = "wasm")]
pub fn html_media_set_sink_id_supported() -> bool {
    use wasm_bindgen::JsValue;
    let present = web_sys::window()
        .and_then(|w| js_sys::Reflect::get(&w, &JsValue::from_str("HTMLMediaElement")).ok())
        .and_then(|ctor| js_sys::Reflect::get(&ctor, &JsValue::from_str("prototype")).ok())
        .map(|proto| js_sys::Reflect::has(&proto, &JsValue::from_str("setSinkId")).unwrap_or(false))
        .unwrap_or(false);
    speaker_selection_supported(present)
}

/// Non-web fallback: no media element, so speaker selection is unsupported.
#[cfg(not(target_family = "wasm"))]
pub fn html_media_set_sink_id_supported() -> bool {
    false
}

/// Load the persisted camera/mic/speaker device IDs. Any unset key yields
/// `None`. The caller is responsible for validating these against the live
/// device list via [`restore_device_id`].
pub fn load_preferred_device_ids() -> (Option<String>, Option<String>, Option<String>) {
    (
        read_local_storage(DEVICE_PREF_CAMERA_KEY),
        read_local_storage(DEVICE_PREF_MIC_KEY),
        read_local_storage(DEVICE_PREF_SPEAKER_KEY),
    )
}

/// Persist the selected camera device ID.
pub fn save_preferred_camera_id(device_id: &str) {
    write_local_storage(DEVICE_PREF_CAMERA_KEY, device_id);
}

/// Persist the selected microphone device ID.
pub fn save_preferred_mic_id(device_id: &str) {
    write_local_storage(DEVICE_PREF_MIC_KEY, device_id);
}

/// Persist the selected speaker (audio-output) device ID.
pub fn save_preferred_speaker_id(device_id: &str) {
    write_local_storage(DEVICE_PREF_SPEAKER_KEY, device_id);
}

/// Load the persisted camera on/off preference. Defaults to `false`
/// (camera off) when no preference has been stored — matching the current
/// pre-join default where camera starts off.
pub fn load_preferred_camera_on() -> bool {
    read_local_storage(DEVICE_PREF_CAMERA_ON_KEY)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Load the persisted mic on/off preference. Defaults to `false` (mic off).
pub fn load_preferred_mic_on() -> bool {
    read_local_storage(DEVICE_PREF_MIC_ON_KEY)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Persist the camera on/off preference.
pub fn save_preferred_camera_on(on: bool) {
    write_local_storage(DEVICE_PREF_CAMERA_ON_KEY, if on { "true" } else { "false" });
}

/// Persist the mic on/off preference.
pub fn save_preferred_mic_on(on: bool) {
    write_local_storage(DEVICE_PREF_MIC_ON_KEY, if on { "true" } else { "false" });
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
///
/// **Semantics:**
///
/// - `WebTransport` (default): attempt WebTransport first; if WebTransport is
///   unavailable, blocked by a firewall, or fails its handshake, automatically
///   fall back to WebSocket. This is what the legacy `Auto` variant did and is
///   the recommended setting for nearly all users.
/// - `WebSocket`: use WebSocket only — no WebTransport attempt is made.
///
/// **Migration**: a persisted value of `"auto"` (the legacy default) is
/// transparently coerced to `WebTransport` by [`FromStr`]. The first time
/// [`load_transport_preference`] sees such a value it logs the migration so
/// operators can verify the upgrade path. The migration is one-shot — on the
/// next storage write the value is canonical.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TransportPreference {
    /// Attempt WebTransport with automatic WebSocket fallback.
    ///
    /// Both URL lists are advertised to the connection manager, which runs
    /// an election preferring WebTransport candidates. When WebTransport is
    /// unavailable (browser support, UDP blocked, server returns non-2xx,
    /// handshake timeout) the manager falls back to the WebSocket candidates.
    #[default]
    WebTransport,
    /// Use WebSocket exclusively — no WebTransport attempt.
    WebSocket,
}

impl std::fmt::Display for TransportPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TransportPreference::WebTransport => "webtransport",
            TransportPreference::WebSocket => "websocket",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for TransportPreference {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            // Legacy "auto" value — migrate to WebTransport. The new
            // WebTransport variant carries the WT-with-WS-fallback semantics
            // that "auto" used to mean.
            "auto" | "webtransport" => Ok(TransportPreference::WebTransport),
            "websocket" => Ok(TransportPreference::WebSocket),
            _ => Err(()),
        }
    }
}

/// Context wrapper for the transport preference signal.
#[derive(Clone, Copy)]
pub struct TransportPreferenceCtx(pub Signal<TransportPreference>);

const TRANSPORT_PREF_KEY: &str = "vc_transport_preference";
const TRANSPORT_STICKY_KEY: &str = "vc_transport_sticky";
const TRANSPORT_SESSION_KEY: &str = "vc_transport_session";

/// Load the persisted transport preference, honouring the sticky flag.
///
/// Resolution order:
///
/// 1. **Sticky enabled** (`vc_transport_sticky == "true"`): read the
///    persistent preference from `localStorage`. This is the explicit
///    "remember my choice" path the user opted into via the Network tab.
/// 2. **Sticky disabled**: any leftover `vc_transport_preference` is treated
///    as stale data from older releases that wrote unconditionally — clear
///    it for backward compatibility, then fall back to `sessionStorage`. The
///    session value is set when the user changes the protocol without ticking
///    "remember", so the change survives the page reload triggered by the
///    select but is forgotten on tab close.
/// 3. Otherwise: `WebTransport` (the new default — was `Auto` before the
///    protocol-settings simplification).
///
/// **Legacy "auto" migration**: when this function reads `"auto"` from
/// storage (the previous default value), it logs the migration once and
/// canonicalises the stored value to `"webtransport"`. The new
/// `WebTransport` variant carries the WT-with-WS-fallback semantics that
/// `Auto` used to mean, so user behaviour is unchanged.
pub fn load_transport_preference() -> TransportPreference {
    let local_storage = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let session_storage = web_sys::window().and_then(|w| w.session_storage().ok().flatten());

    let sticky = local_storage
        .as_ref()
        .and_then(|s| s.get_item(TRANSPORT_STICKY_KEY).ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(false);

    if sticky {
        if let Some(storage) = local_storage.as_ref() {
            if let Ok(Some(raw)) = storage.get_item(TRANSPORT_PREF_KEY) {
                let parsed = raw.parse::<TransportPreference>().ok().unwrap_or_default();
                // Canonicalise the persisted value if it came in as legacy
                // "auto" — the variant is gone, but the stored string would
                // linger otherwise.
                if raw == "auto" {
                    log::info!(
                        "Migrating persisted transport preference \"auto\" -> \"{}\" \
                         (Auto removed in favour of WebTransport-with-WS-fallback)",
                        parsed
                    );
                    let _ = storage.set_item(TRANSPORT_PREF_KEY, &parsed.to_string());
                }
                return parsed;
            }
        }
        return TransportPreference::default();
    }

    // Backward-compat: silently drop a stale persistent preference left over
    // from before the sticky checkbox existed, so previous explicit choices
    // do not "stick" surprise-style after the upgrade.
    if let Some(storage) = local_storage.as_ref() {
        let _ = storage.remove_item(TRANSPORT_PREF_KEY);
    }

    if let Some(storage) = session_storage.as_ref() {
        if let Ok(Some(raw)) = storage.get_item(TRANSPORT_SESSION_KEY) {
            let parsed = raw.parse::<TransportPreference>().ok().unwrap_or_default();
            if raw == "auto" {
                log::info!(
                    "Migrating session transport preference \"auto\" -> \"{}\" \
                     (Auto removed in favour of WebTransport-with-WS-fallback)",
                    parsed
                );
                let _ = storage.set_item(TRANSPORT_SESSION_KEY, &parsed.to_string());
            }
            return parsed;
        }
    }
    TransportPreference::default()
}

/// Persist the transport preference to `localStorage` (the sticky path).
pub fn save_transport_preference(pref: TransportPreference) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(TRANSPORT_PREF_KEY, &pref.to_string());
    }
}

/// Persist the transport preference to `sessionStorage` (the non-sticky path).
///
/// Used when the user changes the protocol without enabling "remember
/// protocol choice" — the value must survive the page reload triggered by
/// the change but should be discarded once the browsing session ends.
pub fn save_transport_preference_session(pref: TransportPreference) {
    if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
        let _ = storage.set_item(TRANSPORT_SESSION_KEY, &pref.to_string());
    }
}

/// Read whether the user has opted to remember the transport choice.
pub fn load_transport_sticky() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(TRANSPORT_STICKY_KEY).ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Persist the sticky flag.
pub fn save_transport_sticky(sticky: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(TRANSPORT_STICKY_KEY, if sticky { "true" } else { "false" });
    }
}

/// Reset all transport-preference storage entries — both the persistent
/// (`localStorage`) keys and the per-session (`sessionStorage`) value — so
/// the next page load resolves to the default (`WebTransport`).
///
/// This is the single source of truth for "go back to default" so callers
/// don't have to know about the three keys involved.
pub fn clear_transport_sticky_and_pref() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(TRANSPORT_STICKY_KEY);
        let _ = storage.remove_item(TRANSPORT_PREF_KEY);
    }
    if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
        let _ = storage.remove_item(TRANSPORT_SESSION_KEY);
    }
}

/// Resolve effective transport configuration from the user's preference and
/// the server-provided WebTransport flag.
///
/// Returns `(enable_webtransport, websocket_urls, webtransport_urls)`.
///
/// **WebTransport-with-WS-fallback**: when the user has selected
/// `WebTransport` (the default), BOTH URL lists are returned. The
/// connection manager creates candidates for every URL and runs an election
/// — if any WebTransport candidate completes its handshake it wins, but if
/// every WT candidate fails (browser support missing, UDP blocked, server
/// rejected the handshake) the WS candidates become the only ones that can
/// be elected and the client automatically uses WebSocket. The fallback is
/// thus structural, not a separate retry: see
/// `videocall-client/src/connection/connection_manager.rs::create_all_connections`.
///
/// `WebSocket` forces a single-transport configuration with the WT list
/// emptied — there is no fallback in that mode by design.
pub fn resolve_transport_config(
    pref: TransportPreference,
    server_wt_enabled: bool,
    ws_urls: Vec<String>,
    wt_urls: Vec<String>,
) -> (bool, Vec<String>, Vec<String>) {
    match pref {
        // WebTransport selection ≡ legacy Auto: surface BOTH URL lists so
        // the manager's election can fall back to WebSocket if every WT
        // candidate fails. The `server_wt_enabled` flag still gates whether
        // the manager will attempt the WT URLs at all (e.g. when runtime
        // config hasn't loaded yet) — this is unchanged from Auto behaviour.
        TransportPreference::WebTransport => (server_wt_enabled, ws_urls, wt_urls),
        TransportPreference::WebSocket => (false, ws_urls, vec![]),
    }
}

/// Handle a transport preference change from transport selection controls.
///
/// Shows a confirmation dialog. If the user confirms, persists the preference
/// (sticky vs session-only depending on `sticky`) and reloads the page. If
/// cancelled, attempts to reset a native `<select>` control (when present)
/// back to the current value so it doesn't appear stale.
///
/// Routing rules:
///
/// - The default (`WebTransport`) selected with `sticky == false`: clear every
///   transport-preference storage key so the next load resolves to the default
///   without needing a remembered choice.
/// - Selecting any value with `sticky == true`: write to `localStorage` so the
///   choice persists across browser sessions.
/// - Non-default selection with `sticky == false`: write to `sessionStorage`
///   so the choice survives the imminent page reload but evaporates when the
///   tab closes.
///
/// Custom controls (like the settings modal glass dropdown) are state-driven
/// and naturally re-render with the current value when the user cancels.
pub fn confirm_transport_change(
    new_value: &str,
    current: TransportPreference,
    select_id: &str,
    sticky: bool,
) {
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
        let is_default = pref == TransportPreference::default();
        match (is_default, sticky) {
            // Default + not sticky: clear all storage — implicit default
            // doesn't need to be remembered.
            (true, false) => clear_transport_sticky_and_pref(),
            (_, true) => {
                save_transport_preference(pref);
                save_transport_sticky(true);
            }
            (false, false) => save_transport_preference_session(pref),
        }
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
    email_to_display_name, is_allowed_display_name_char, is_guid_like, is_valid_meeting_id,
    validate_display_name, DISPLAY_NAME_MAX_LEN,
};

// ── Theme preference ──────────────────────────────────────────────────────────

/// Application colour theme.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    #[default]
    Dark,
    /// Follow the OS `prefers-color-scheme` media query.
    System,
    Light,
}

impl Theme {
    /// Stored value written to localStorage.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::System => "system",
            Theme::Light => "light",
        }
    }

    /// Label shown in the UI toggle.
    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::System => "System",
            Theme::Light => "Light",
        }
    }
}

impl std::str::FromStr for Theme {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "system" => Ok(Theme::System),
            "light" | "dawn" => Ok(Theme::Light),
            _ => Ok(Theme::Dark),
        }
    }
}

/// Context providing the active theme signal to the component tree.
#[derive(Clone, Copy)]
pub struct ThemePreferenceCtx(pub Signal<Theme>);

const THEME_STORAGE_KEY: &str = "ui-theme";

/// Load theme from localStorage; falls back to `Theme::Dark`.
pub fn load_theme_from_storage() -> Theme {
    LocalStorage::get::<String>(&THEME_STORAGE_KEY.to_string())
        .and_then(|v| v.parse().ok())
        .unwrap_or_default()
}

/// Apply `data-theme` on `<html>` without touching localStorage.
/// Use this for the FOUC-prevention path where the value is already loaded.
/// When `theme` is `Theme::System` the resolved value is read from the
/// `prefers-color-scheme` media query at call time.
pub fn apply_theme_to_dom(theme: Theme) {
    let resolved = match theme {
        Theme::System => {
            let prefers_dark = web_sys::window()
                .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
                .map(|mql| mql.matches())
                .unwrap_or(true);
            if prefers_dark {
                "dark"
            } else {
                "light"
            }
        }
        _ => theme.as_str(),
    };
    if let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = root.set_attribute("data-theme", resolved);
    }
}

/// Persist theme to localStorage and apply `data-theme` on `<html>`.
pub fn apply_and_save_theme(theme: Theme) {
    LocalStorage::set(THEME_STORAGE_KEY.to_string(), &theme.as_str().to_string());
    apply_theme_to_dom(theme);
}

/// Handle to a `(prefers-color-scheme: dark)` MediaQueryList `change` listener.
///
/// Keeping the [`Closure`] alive as a field is what prevents JS from
/// reclaiming the underlying callback while the listener is still
/// registered.  `remove()` detaches the listener — call it on app unmount
/// so we never accumulate dangling listeners across hot reloads.
pub struct PrefersColorSchemeHandle {
    closure: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    mql: web_sys::MediaQueryList,
}

impl PrefersColorSchemeHandle {
    pub fn remove(&self) {
        use wasm_bindgen::JsCast;
        let _ = self
            .mql
            .remove_event_listener_with_callback("change", self.closure.as_ref().unchecked_ref());
    }
}

/// Subscribe to OS-level `prefers-color-scheme` changes.
///
/// While [`Theme::System`] is the active preference, an OS dark↔light
/// switch (e.g. macOS sunset, manual iOS toggle) re-runs
/// [`apply_theme_to_dom`] so the page follows the OS without a reload.
/// For [`Theme::Dark`] / [`Theme::Light`] the change is ignored — the user
/// has expressed an explicit preference.
pub fn register_prefers_color_scheme_listener(
    theme: Signal<Theme>,
) -> Option<std::rc::Rc<PrefersColorSchemeHandle>> {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let mql = web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())?;

    let theme_signal = theme;
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |_evt: web_sys::Event| {
        if matches!(*theme_signal.peek(), Theme::System) {
            apply_theme_to_dom(Theme::System);
        }
    });

    mql.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
        .ok()?;

    Some(std::rc::Rc::new(PrefersColorSchemeHandle { closure, mql }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_position_css_class() {
        assert_eq!(DockPosition::Bottom.css_class(), "dock-bottom");
        assert_eq!(DockPosition::Left.css_class(), "dock-left");
        assert_eq!(DockPosition::Right.css_class(), "dock-right");
    }

    #[test]
    fn dock_position_next_cycles() {
        assert_eq!(DockPosition::Bottom.next(), DockPosition::Left);
        assert_eq!(DockPosition::Left.next(), DockPosition::Right);
        assert_eq!(DockPosition::Right.next(), DockPosition::Bottom);
    }

    #[test]
    fn dock_position_next_full_roundtrip() {
        let start = DockPosition::Bottom;
        let result = start.next().next().next();
        assert_eq!(result, start);
    }

    #[test]
    fn density_mode_labels() {
        assert_eq!(DensityMode::Auto.label(), "Auto");
        assert_eq!(DensityMode::Standard.label(), "Standard");
        assert_eq!(DensityMode::Dense.label(), "Dense");
        assert_eq!(DensityMode::Maximum.label(), "Maximum");
    }

    #[test]
    fn density_mode_debug_impl() {
        assert_eq!(format!("{:?}", DensityMode::Auto), "Auto");
        assert_eq!(format!("{:?}", DensityMode::Standard), "Standard");
        assert_eq!(format!("{:?}", DensityMode::Dense), "Dense");
        assert_eq!(format!("{:?}", DensityMode::Maximum), "Maximum");
    }

    #[test]
    fn density_mode_clone_and_eq() {
        let original = DensityMode::Dense;
        let cloned = original;
        assert_eq!(original, cloned);

        assert_ne!(DensityMode::Auto, DensityMode::Standard);
        assert_ne!(DensityMode::Dense, DensityMode::Maximum);
        assert_ne!(DensityMode::Auto, DensityMode::Maximum);
    }

    #[test]
    fn density_mode_ctx_clone() {
        // Compile-time check that DensityModeCtx implements Clone.
        fn _assert_clone<T: Clone>() {}
        _assert_clone::<DensityModeCtx>();
    }

    #[test]
    fn decode_budget_override_default_is_auto() {
        assert_eq!(DecodeBudgetOverride::default(), DecodeBudgetOverride::Auto);
    }

    #[test]
    fn decode_budget_override_debug_impl() {
        assert_eq!(format!("{:?}", DecodeBudgetOverride::Auto), "Auto");
        assert_eq!(format!("{:?}", DecodeBudgetOverride::Fixed(4)), "Fixed(4)");
    }

    #[test]
    fn decode_budget_override_clone_and_eq() {
        let original = DecodeBudgetOverride::Fixed(8);
        let cloned = original;
        assert_eq!(original, cloned);

        assert_ne!(DecodeBudgetOverride::Auto, DecodeBudgetOverride::Fixed(1));
        assert_ne!(
            DecodeBudgetOverride::Fixed(2),
            DecodeBudgetOverride::Fixed(3)
        );
    }

    #[test]
    fn decode_budget_override_ctx_clone() {
        // Compile-time check that DecodeBudgetCtx implements Clone.
        fn _assert_clone<T: Clone>() {}
        _assert_clone::<DecodeBudgetCtx>();
    }

    #[test]
    fn decode_budget_override_roundtrip_auto() {
        let serialized = serialize_decode_budget_override(DecodeBudgetOverride::Auto);
        assert_eq!(serialized, "auto");
        assert_eq!(
            parse_decode_budget_override(&serialized),
            DecodeBudgetOverride::Auto
        );
    }

    #[test]
    fn decode_budget_override_roundtrip_fixed() {
        let serialized = serialize_decode_budget_override(DecodeBudgetOverride::Fixed(12));
        assert_eq!(serialized, "12");
        assert_eq!(
            parse_decode_budget_override(&serialized),
            DecodeBudgetOverride::Fixed(12)
        );
    }

    #[test]
    fn decode_budget_override_parse_invalid_falls_back_to_auto() {
        // Garbage, empty, negative, and zero all collapse to the default Auto,
        // mirroring the density-mode "_ => Auto" fallback semantics.
        assert_eq!(
            parse_decode_budget_override("garbage"),
            DecodeBudgetOverride::Auto
        );
        assert_eq!(parse_decode_budget_override(""), DecodeBudgetOverride::Auto);
        assert_eq!(
            parse_decode_budget_override("-5"),
            DecodeBudgetOverride::Auto
        );
        assert_eq!(
            parse_decode_budget_override("0"),
            DecodeBudgetOverride::Auto
        );
    }

    #[test]
    fn resolve_dock_autohide_defaults_to_false_when_unset() {
        // First-time users with no persisted preference should see the
        // action bar always visible — autohide must default to false.
        assert!(!resolve_dock_autohide(None));
    }

    #[test]
    fn resolve_dock_autohide_honors_stored_true() {
        // Regression guard: a user who has explicitly enabled autohide must
        // keep that preference after the default-off fix.
        assert!(resolve_dock_autohide(Some("true")));
    }

    #[test]
    fn resolve_dock_autohide_honors_stored_false() {
        // Explicitly disabled autohide stays disabled.
        assert!(!resolve_dock_autohide(Some("false")));
    }

    // -----------------------------------------------------------------------
    // Pre-join device preference helpers (issue #959)
    // -----------------------------------------------------------------------

    #[test]
    fn restore_device_id_uses_stored_when_present() {
        let available = vec!["cam-a".to_string(), "cam-b".to_string()];
        assert_eq!(
            restore_device_id(Some("cam-b"), &available),
            Some("cam-b".to_string())
        );
    }

    #[test]
    fn restore_device_id_falls_back_when_stored_missing() {
        // Stored device was unplugged between visits → fall back to first.
        let available = vec!["cam-a".to_string(), "cam-b".to_string()];
        assert_eq!(
            restore_device_id(Some("cam-gone"), &available),
            Some("cam-a".to_string())
        );
    }

    #[test]
    fn restore_device_id_falls_back_when_none_stored() {
        let available = vec!["cam-a".to_string()];
        assert_eq!(
            restore_device_id(None, &available),
            Some("cam-a".to_string())
        );
    }

    #[test]
    fn restore_device_id_empty_stored_falls_back() {
        let available = vec!["cam-a".to_string()];
        assert_eq!(
            restore_device_id(Some(""), &available),
            Some("cam-a".to_string())
        );
    }

    #[test]
    fn restore_device_id_no_devices_returns_none() {
        let available: Vec<String> = vec![];
        assert_eq!(restore_device_id(Some("cam-a"), &available), None);
        assert_eq!(restore_device_id(None, &available), None);
    }

    #[test]
    fn resolve_initial_enabled_requires_all_conditions() {
        // The happy path: stored on + permission + device present → on.
        assert!(resolve_initial_enabled(true, true, true));
    }

    #[test]
    fn resolve_initial_enabled_off_when_stored_off() {
        assert!(!resolve_initial_enabled(false, true, true));
    }

    #[test]
    fn resolve_initial_enabled_off_when_permission_denied() {
        // Never enable capture we are not allowed to perform.
        assert!(!resolve_initial_enabled(true, false, true));
    }

    #[test]
    fn resolve_initial_enabled_off_when_no_device() {
        assert!(!resolve_initial_enabled(true, true, false));
    }

    #[test]
    fn speaker_selection_supported_tracks_capability_flag() {
        assert!(speaker_selection_supported(true));
        assert!(!speaker_selection_supported(false));
    }
}
