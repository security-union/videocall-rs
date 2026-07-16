// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context providers for the application
//!
//! This module centralises shared state that needs to be accessed across
//! the component tree through Dioxus context providers.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use videocall_client::VideoCallClient;

/// Per-tile crop state: canvas ID → is-cropped.
/// Survives re-renders caused by peer list changes so crop toggles persist.
#[derive(Clone, Copy)]
pub struct CroppedTilesCtx(pub Signal<std::collections::HashMap<String, bool>>);

/// Issue 1175: per-tile zoom / pan state for a RECEIVED shared-content tile.
///
/// `scale` is the CSS transform scale applied to the content wrapper (1.0 ==
/// fit-to-tile, the resting state; clamped to `[1.0, 4.0]` by
/// `screen_share_zoom`). `off_x` / `off_y` are the pan translation in CSS
/// pixels applied together with the scale. An entry absent from the map is the
/// default fit state (`scale = 1.0`, no pan), so the common un-zoomed tile
/// stores nothing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScreenZoomState {
    pub scale: f64,
    pub off_x: f64,
    pub off_y: f64,
}

impl Default for ScreenZoomState {
    fn default() -> Self {
        Self {
            scale: 1.0,
            off_x: 0.0,
            off_y: 0.0,
        }
    }
}

/// Issue 1175: per-tile zoom/pan state for received shared content, keyed by
/// peer session id. Mirrors [`CroppedTilesCtx`] deliberately — a single shared
/// signal so a zoom change re-renders only the affected screen-share tile
/// (there is at most one active sharer) and survives peer-list re-renders. The
/// state is rendered DECLARATIVELY as a CSS `transform` on a wrapper around the
/// canvas, so the `<canvas>` node the decoder paints into is never recreated
/// (the whole point of the issue-1175 v2 rewrite).
#[derive(Clone, Copy)]
pub struct ScreenZoomCtx(pub Signal<std::collections::HashMap<String, ScreenZoomState>>);

/// Issue 1175: the single peer whose shared content is currently detached into
/// a separate window, or `None`. One-at-a-time by design (the Document
/// Picture-in-Picture API allows only one window; the `window.open` fallback
/// keeps the same invariant). Drives the `.share-detached` class on
/// `#grid-container`, which hides the split share pane OFF-SCREEN (and marks it
/// `inert`) so the main window looks like a regular no-share meeting — while the
/// canvas stays mounted, composited, and painting so the detached-window mirror
/// keeps flowing and reattach is instant.
#[derive(Clone, Copy)]
pub struct DetachedShareCtx(pub Signal<Option<String>>);

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
///
/// `All` (issue #1466) is a second hard override meaning "decode all the tiles
/// the layout would show". Like `Fixed(n)` it bypasses the adaptive loop, but
/// instead of a literal count it tracks the live natural tile count, so it
/// stays correct as peers join/leave. It is the persistent "show all paused
/// videos" escape hatch reachable from the Appearance/Settings panel,
/// independent of the banner's `pressured && avatar_count > 0` gate. The #1286
/// iOS device ceiling STILL binds on `All` (see `effective_cap`): "All" means
/// "everything the layout shows, still subject to the hardware ceiling", never
/// a ceiling bypass.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DecodeBudgetOverride {
    #[default]
    Auto,
    Fixed(usize),
    /// Issue #1466: decode every natural tile (clamped at `CANVAS_LIMIT` and the
    /// #1286 device ceiling). A hard override like `Fixed`, but count-free.
    All,
}

/// Context for the decode-budget override.
#[derive(Clone, Copy)]
pub struct DecodeBudgetCtx(pub Signal<DecodeBudgetOverride>);

/// Issue #1466: the set of peer `session_id`s the local user has explicitly
/// asked to keep decoding via the per-tile PLAY button, even when the decode
/// budget would otherwise pause (avatar) them.
///
/// Holds `session_id`s (the `key`/`peer_id` `generate_for_peer` receives, which
/// `parse::<u64>()` cleanly into the wire id). The merge into `active_decode_set`
/// is the single union point (see `decode_budget::merge_user_requested_decode`).
///
/// NOT persisted to `localStorage` — deliberately. Each entry is a per-session,
/// transient request bounded by the live peer set: a `session_id` is unique to
/// one browser connection and is regenerated on every reload, so a persisted id
/// would be stale (match no live peer) the moment the page reloads. Persisting
/// it would therefore be inert at best and confusing at worst, so this state
/// lives only in render-scope signal memory and is cleaned up when its peer
/// leaves (`attendants.rs` stale-request prune).
///
/// The parent (`AttendantsComponent`) owns the backing signal directly and
/// threads a `toggle` `EventHandler` down to the per-tile PLAY button, so the
/// current wiring does not read this context back out — it is provided for API
/// symmetry with the other decode-budget contexts and for future child access.
/// Hence `#[allow(dead_code)]` on the field (mirrors `LocalAudioLevelCtx`).
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct UserRequestedDecodeCtx(pub Signal<std::collections::HashSet<String>>);

/// Issue #1558: the live protective-mode report, published by the decode-budget
/// control loop in `AttendantsComponent` and consumed by `Host` to actuate the
/// LOCAL encoder send-layer self-shed (stage 3).
///
/// Protective mode is a thin layer ON TOP of the #1557 decode cascade: when the
/// client is in sustained distress (low FPS / saturated main thread / audio
/// buffer backing up / low-cap + crowded), the loop latches `active` true and —
/// once the cascade reaches floor — requests `encoder_layer_ceiling` (a layer
/// COUNT, 2 then 1) to shed the LOCAL encoder's send ladder and free CPU for
/// decode + audio. `Host` composes this ceiling with the user's persisted "layers
/// published" preference via `min` (so neither clobbers the other) and applies it
/// through the existing `set_user_layer_ceiling` actuator. When protective mode
/// exits, `encoder_layer_ceiling` reverts to `None` and `Host` restores the
/// user's preference alone — full reversibility.
///
/// The decode-side stages (cascade layers→pause, emergency non-speaker pause) are
/// actuated by the loop itself via `decode_budget_cap`; only the ENCODER stage
/// crosses the component boundary, hence this is the sole field that needs the
/// context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ProtectiveModeReport {
    /// True while protective mode is latched on.
    pub active: bool,
    /// The LOCAL encoder send-layer ceiling protective mode requests (a layer
    /// COUNT, floored at 1 = base-only), or `None` when no encoder shed is
    /// requested (inactive, or active but the cascade has not reached floor).
    pub encoder_layer_ceiling: Option<u32>,
}

/// Context wrapper for [`ProtectiveModeReport`] (issue #1558).
#[derive(Clone, Copy)]
pub struct ProtectiveModeCtx(pub Signal<ProtectiveModeReport>);

const DECODE_BUDGET_OVERRIDE_KEY: &str = "vc_decode_budget_override";

/// Parse a persisted decode-budget override string. Mirrors the density-mode
/// manual-match style: `"auto"` (or any unparseable value) yields the default
/// `Auto`; a positive integer string yields `Fixed(n)`. A stored `Fixed(0)`
/// (or any value that fails to parse as a non-zero `usize`) collapses to
/// `Auto`, since a zero-tile hard override is meaningless.
fn parse_decode_budget_override(raw: &str) -> DecodeBudgetOverride {
    match raw {
        "auto" => DecodeBudgetOverride::Auto,
        // Issue #1466: the persistent "show all paused videos" choice.
        "all" => DecodeBudgetOverride::All,
        other => match other.parse::<usize>() {
            Ok(n) if n > 0 => DecodeBudgetOverride::Fixed(n),
            _ => DecodeBudgetOverride::Auto,
        },
    }
}

/// Serialize a decode-budget override to its compact storage string: `"auto"`
/// for `Auto`, `"all"` for `All` (issue #1466), or the bare integer for
/// `Fixed(n)`.
fn serialize_decode_budget_override(value: DecodeBudgetOverride) -> String {
    match value {
        DecodeBudgetOverride::Auto => "auto".to_string(),
        DecodeBudgetOverride::All => "all".to_string(),
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
        DecodeBudgetOverride::All => {
            log::info!("DecodeBudget: override=all source=user_setting")
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
                let (r, g, b) = crate::util::color_math::parse_hex(&other[7..])?;
                Some(GlowColor::Custom { r, g, b })
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

    #[allow(dead_code)] // strict-form entry point kept as public API; tests cover it
    pub fn from_hex(hex: &str) -> Option<Self> {
        // Strict form: exactly `#RRGGBB`. Callers with lenient input (trimmed
        // whitespace, missing `#`) should parse via `color_math::parse_hex`
        // and hand the RGB to `from_rgb` instead.
        if hex.len() != 7 || !hex.starts_with('#') {
            return None;
        }
        let (r, g, b) = crate::util::color_math::parse_hex(hex)?;
        Some(Self::from_rgb(r, g, b))
    }

    /// Return the preset that matches `(r, g, b)` if any, otherwise wrap in
    /// `GlowColor::Custom`. Single source of truth for preset-vs-custom
    /// disambiguation — call this instead of stringifying to hex and going
    /// back through `from_hex`.
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        let presets = [
            GlowColor::White,
            GlowColor::Cyan,
            GlowColor::Magenta,
            GlowColor::Plum,
            GlowColor::MintGreen,
        ];
        for preset in presets {
            if preset.to_rgb() == (r, g, b) {
                return preset;
            }
        }
        GlowColor::Custom { r, g, b }
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
    pub glow_decay: f32,          // 0.0–1.0 scale factor
    pub show_entry_notifications: bool,
    pub show_exit_notifications: bool,
    pub play_entry_sound: bool,
    pub play_exit_sound: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        AppearanceSettings {
            glow_enabled: true,
            glow_color: GlowColor::MintGreen,
            glow_brightness: 0.5,
            inner_glow_strength: 0.5,
            glow_decay: 0.5,
            show_entry_notifications: true,
            show_exit_notifications: true,
            play_entry_sound: true,
            play_exit_sound: true,
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
const APPEARANCE_DECAY_STORAGE_KEY: &str = "vc_appearance_glow_decay";
const APPEARANCE_ENTRY_NOTIFICATIONS_KEY: &str = "vc_appearance_entry_notifications";
const APPEARANCE_EXIT_NOTIFICATIONS_KEY: &str = "vc_appearance_exit_notifications";
const APPEARANCE_ENTRY_SOUND_KEY: &str = "vc_appearance_entry_sound";
const APPEARANCE_EXIT_SOUND_KEY: &str = "vc_appearance_exit_sound";
const CUSTOM_COLORS_STORAGE_KEY: &str = "vc_appearance_custom_colors";

pub const MAX_CUSTOM_COLORS: usize = 10;

/// Load local-only appearance settings from storage.
///
/// Returns defaults for any missing or invalid values.
pub fn load_appearance_settings_from_storage() -> AppearanceSettings {
    let mut settings = AppearanceSettings::default();

    if let Some(value) = read_local_storage(APPEARANCE_GLOW_ENABLED_STORAGE_KEY) {
        settings.glow_enabled = value != "false";
    }

    if let Some(color) = read_local_storage(APPEARANCE_COLOR_STORAGE_KEY) {
        if let Some(parsed) = GlowColor::from_storage(&color) {
            settings.glow_color = parsed;
        }
    }

    if let Some(value) =
        read_local_storage(APPEARANCE_BRIGHTNESS_STORAGE_KEY).and_then(|v| v.parse::<f32>().ok())
    {
        settings.glow_brightness = value.clamp(0.0, 1.0);
    }

    if let Some(value) =
        read_local_storage(APPEARANCE_INNER_STORAGE_KEY).and_then(|v| v.parse::<f32>().ok())
    {
        settings.inner_glow_strength = value.clamp(0.0, 1.0);
    }

    if let Some(value) =
        read_local_storage(APPEARANCE_DECAY_STORAGE_KEY).and_then(|v| v.parse::<f32>().ok())
    {
        settings.glow_decay = value.clamp(0.0, 1.0);
    }

    apply_notification_prefs(
        &mut settings,
        read_local_storage(APPEARANCE_ENTRY_NOTIFICATIONS_KEY).as_deref(),
        read_local_storage(APPEARANCE_EXIT_NOTIFICATIONS_KEY).as_deref(),
        read_local_storage(APPEARANCE_ENTRY_SOUND_KEY).as_deref(),
        read_local_storage(APPEARANCE_EXIT_SOUND_KEY).as_deref(),
    );

    settings
}

/// Resolve the entry/exit message + sound preferences onto `settings` from the
/// raw stored strings (as returned by [`read_local_storage`], i.e. plain text —
/// these keys are NOT CBOR/zlib encoded). `None` means the key is absent, in
/// which case that direction keeps its incoming (default-on) value.
///
/// A value equal to `"false"` disables; any other present value enables
/// (mirrors the `!= "false"` read used elsewhere for boolean prefs). Each of
/// the four toggles is applied independently.
///
/// Extracted as a pure function so the per-direction gating is unit testable
/// without a DOM — the E2E harness cannot observe the exit (leave) direction
/// because the "left the meeting" toast is currently suppressed there (see the
/// skipped leave-toast tests in `toast-notifications.spec.ts`). Tested via
/// `#[wasm_bindgen_test]` in `tests/context_unit.rs` (the lib's plain `#[test]`
/// block is not executed by the wasm test runner).
pub fn apply_notification_prefs(
    settings: &mut AppearanceSettings,
    entry_notifications: Option<&str>,
    exit_notifications: Option<&str>,
    entry_sound: Option<&str>,
    exit_sound: Option<&str>,
) {
    if let Some(value) = entry_notifications {
        settings.show_entry_notifications = value != "false";
    }
    if let Some(value) = exit_notifications {
        settings.show_exit_notifications = value != "false";
    }
    if let Some(value) = entry_sound {
        settings.play_entry_sound = value != "false";
    }
    if let Some(value) = exit_sound {
        settings.play_exit_sound = value != "false";
    }
}

/// Save local-only appearance settings to storage.
pub fn save_appearance_settings_to_storage(settings: &AppearanceSettings) {
    write_local_storage(
        APPEARANCE_GLOW_ENABLED_STORAGE_KEY,
        &settings.glow_enabled.to_string(),
    );
    write_local_storage(
        APPEARANCE_COLOR_STORAGE_KEY,
        &settings.glow_color.to_storage(),
    );
    write_local_storage(
        APPEARANCE_BRIGHTNESS_STORAGE_KEY,
        &settings.glow_brightness.clamp(0.0, 1.0).to_string(),
    );
    write_local_storage(
        APPEARANCE_INNER_STORAGE_KEY,
        &settings.inner_glow_strength.clamp(0.0, 1.0).to_string(),
    );
    write_local_storage(
        APPEARANCE_DECAY_STORAGE_KEY,
        &settings.glow_decay.clamp(0.0, 1.0).to_string(),
    );
    write_local_storage(
        APPEARANCE_ENTRY_NOTIFICATIONS_KEY,
        &settings.show_entry_notifications.to_string(),
    );
    write_local_storage(
        APPEARANCE_EXIT_NOTIFICATIONS_KEY,
        &settings.show_exit_notifications.to_string(),
    );
    write_local_storage(
        APPEARANCE_ENTRY_SOUND_KEY,
        &settings.play_entry_sound.to_string(),
    );
    write_local_storage(
        APPEARANCE_EXIT_SOUND_KEY,
        &settings.play_exit_sound.to_string(),
    );
}

/// Load custom glow colors from local storage.
pub fn load_custom_colors_from_storage() -> Vec<GlowColor> {
    let Some(csv) = read_local_storage(CUSTOM_COLORS_STORAGE_KEY) else {
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
    write_local_storage(CUSTOM_COLORS_STORAGE_KEY, &csv);
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

/// Reactive set of the `user_id`(s) currently holding host in the meeting.
///
/// Single-host model, so this holds at most one entry — but a `HashSet` keeps
/// the update path trivial and order-free. Transfer-host moves host between
/// participants, and `host_user_id` (= the meeting CREATOR) is stale once host
/// has been transferred away, so the crown / "(Host)" indicator is driven by
/// this set instead. Seeded authoritatively from the `/participants` roster and
/// updated live on `HOST_GRANTED` / `HOST_REVOKED` broadcasts, so every client
/// paints the crown on the current host without a reload.
#[derive(Clone, Copy)]
pub struct HostSetCtx(pub Signal<std::collections::HashSet<String>>);

/// Nonce bumped by `AttendantsComponent` when the LOCAL user is granted or has
/// their host revoked. `MeetingPage` watches it, re-fetches the participant
/// status (which re-signs the room token from the live DB `is_host`), and
/// updates its `MeetingStatus::Admitted`. That flips the `is_owner` prop and
/// re-renders `AttendantsComponent` IN PLACE — deliberately WITHOUT a `key`, so
/// the media client is NOT torn down and the user is not bounced back to the
/// join screen. Host REST actions authorize on the session + DB `is_host`, and
/// the media server learns the new host via the `meeting_host_changed` NATS
/// fanout, so no reconnect is required.
#[derive(Clone, Copy)]
pub struct HostRefreshNonceCtx(pub Signal<u64>);

impl HostSetCtx {
    /// Whether `user_id` is currently a host.
    pub fn is_host(&self, user_id: &str) -> bool {
        self.0.read().contains(user_id)
    }
}

// ---------------------------------------------------------------------------
// Local-storage helpers
// ---------------------------------------------------------------------------

const STORAGE_KEY: &str = "vc_display_name";

/// Secondary plain-text key that bypasses CBOR+zlib serialization.  On Safari,
/// Load the persisted display name from local storage.
///
/// Reads the plain-text value stored under [`STORAGE_KEY`] in the browser's
/// `localStorage`.  Returns `None` when no name has been saved yet, or when
/// the stored value is empty.
pub fn load_display_name_from_storage() -> Option<String> {
    read_local_storage(STORAGE_KEY)
}

/// Persist the display name to local storage.
pub fn save_display_name_to_storage(display_name: &str) {
    write_local_storage(STORAGE_KEY, display_name);
}

/// Remove the display name from local storage entirely (e.g. on logout).
pub fn clear_display_name_from_storage() {
    remove_local_storage(STORAGE_KEY);
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

fn remove_local_storage(key: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(key);
    }
}

// ---------------------------------------------------------------------------
// Session-storage helpers (plain-text, no CBOR/zlib)
// ---------------------------------------------------------------------------

/// Read a plain-text value from the browser's `sessionStorage`.
///
/// Returns `None` when `sessionStorage` is unavailable, the key is missing,
/// or the stored value is empty.
pub(crate) fn read_session_storage(key: &str) -> Option<String> {
    web_sys::window()
        .and_then(|w| w.session_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
        .filter(|v| !v.is_empty())
}

/// Write a plain-text value to the browser's `sessionStorage`.
/// Silently ignores failures (e.g. Safari private mode, quota).
pub(crate) fn write_session_storage(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}

/// Remove a key from the browser's `sessionStorage`.
/// Silently ignores failures.
pub(crate) fn remove_session_storage(key: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
        let _ = storage.remove_item(key);
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
/// and persist it in `localStorage` so the same browser/device always
/// presents the same identity.
pub fn get_or_create_local_user_id() -> String {
    if let Some(id) = read_local_storage(USER_ID_STORAGE_KEY) {
        return id;
    }
    let id = generate_local_id();
    write_local_storage(USER_ID_STORAGE_KEY, &id);
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

/// Sentinel key written after the one-time legacy migration completes.
/// Prevents `migrate_legacy_storage` from re-running on every startup, which
/// would perpetually wipe any legitimate all-hex display name (e.g. "1234").
#[cfg(target_family = "wasm")]
const MIGRATION_DONE_KEY: &str = "vc_storage_migrated";

/// One-time migration of legacy display-name storage formats to plain text.
///
/// Previous builds stored `vc_display_name` via `dioxus_sdk_storage` which
/// serialises as **CBOR → zlib → lowercase hex**. The resulting localStorage
/// value is an even-length string of ASCII hex digits (`[0-9a-f]`), *not*
/// binary — so it looks printable but is not a valid display name.
///
/// A real zlib stream is at least 6 bytes (2-byte header + empty DEFLATE +
/// 4-byte Adler-32), and CBOR wrapping adds more, so the hex representation
/// of even the shortest name is ≥ 16 hex chars.  We use that as the length
/// floor to avoid false-positives on short numeric/hex names like "1234".
/// As a second discriminator, the first two hex chars are decoded and checked
/// against the zlib magic: first byte must be `0x78` (deflate, 32K window)
/// and the two-byte big-endian value must be divisible by 31.
///
/// A later Safari ITP fix added a parallel plain-text key
/// `vc_display_name_raw`. Very old releases used `vc_username`.
///
/// This function migrates those legacy formats to the current plain-text
/// `vc_display_name` key and writes a sentinel so it never runs again.
/// Users who only had CBOR-encoded data (without the parallel raw key) will
/// need to re-enter their name once — acceptable since the raw key was
/// written alongside CBOR for some time.
///
/// **Note on appearance settings** (`vc_appearance_glow_brightness`, etc.):
/// old CBOR-encoded f32 values are also hex blobs that `parse::<f32>()` will
/// reject, causing a silent reset to defaults. This is acceptable — the
/// correct value self-heals on the next user save.
///
/// Must be called at app startup **before** the Dioxus component tree mounts.
/// It is a no-op after the first successful run, when the primary key already
/// has a readable value, or on non-web platforms.
pub fn migrate_legacy_storage() {
    #[cfg(target_family = "wasm")]
    {
        // If migration already ran once, nothing to do.
        if read_local_storage(MIGRATION_DONE_KEY).is_some() {
            return;
        }

        // If the primary key exists, check whether it is a legacy hex blob.
        //
        // dioxus_sdk_storage encodes as CBOR → zlib → lowercase hex.  A real
        // zlib stream is ≥ 6 bytes → ≥ 12 hex chars; with the CBOR envelope
        // the realistic minimum is ~18-30 hex chars.  We use a floor of 16 to
        // avoid false-positives on short legitimate names like "1234" or
        // "cafe".
        //
        // As a secondary check we verify the zlib magic: the first byte must
        // be 0x78 (deflate, 32K window) and the two-byte header interpreted
        // as a big-endian u16 must be divisible by 31.
        if let Some(v) = read_local_storage(STORAGE_KEY) {
            let is_legacy_hex_blob = v.len() >= 16
                && v.len() % 2 == 0
                && v.chars().all(|c| c.is_ascii_hexdigit())
                && has_zlib_magic(&v);
            if !is_legacy_hex_blob {
                // Genuinely plain text (or too short / wrong header to be a
                // zlib stream) — mark migration done and return.
                write_local_storage(MIGRATION_DONE_KEY, "1");
                return;
            }
            // Legacy hex blob — drop it and fall through to fallback keys.
            remove_local_storage(STORAGE_KEY);
        }

        // Try the plain-text fallback key added in the Safari ITP fix.
        // This covers users who saved their name while both keys were written.
        if let Some(v) = read_local_storage("vc_display_name_raw") {
            write_local_storage(STORAGE_KEY, &v);
            remove_local_storage("vc_display_name_raw");
            write_local_storage(MIGRATION_DONE_KEY, "1");
            return;
        }

        // Try the legacy "vc_username" key from very old releases.
        if let Some(v) = read_local_storage("vc_username") {
            write_local_storage(STORAGE_KEY, &v);
            remove_local_storage("vc_username");
        }

        // Mark migration complete so this function becomes a no-op on
        // subsequent startups.
        write_local_storage(MIGRATION_DONE_KEY, "1");
    }
}

/// Check whether the first two bytes of a hex string match the zlib magic.
///
/// A valid zlib header starts with `0x78` (CMF: deflate method, 32K window)
/// and the two-byte CMF+FLG value interpreted as big-endian u16 must satisfy
/// `value % 31 == 0`.  We decode the first 4 hex chars (= 2 bytes) and apply
/// both checks.  Returns `false` on any parse failure, keeping the
/// false-positive rate near zero without pulling in a zlib dependency.
#[cfg(target_family = "wasm")]
fn has_zlib_magic(hex: &str) -> bool {
    if hex.len() < 4 {
        return false;
    }
    let Ok(b0) = u8::from_str_radix(&hex[..2], 16) else {
        return false;
    };
    let Ok(b1) = u8::from_str_radix(&hex[2..4], 16) else {
        return false;
    };
    // CMF byte must be 0x78 (deflate, 32K window size).
    if b0 != 0x78 {
        return false;
    }
    // The two-byte header (big-endian) must be divisible by 31.
    let header = (b0 as u16) << 8 | b1 as u16;
    header.is_multiple_of(31)
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

/// Persist a transport-preference decision to storage, with no UI side effects.
///
/// This is the single source of truth for "given a chosen protocol and whether
/// the user asked to remember it, what should each storage area end up
/// holding". Both the settings-modal "Apply" button and
/// [`confirm_transport_change`] call this so their persistence logic cannot
/// drift. It deliberately does NOT prompt (`window.confirm`) or reload — those
/// stay in the callers.
///
/// End-state per arm (`pref` is the chosen protocol, default is `WebTransport`):
///
/// - **default + not sticky** (`(true, false)`): clear every key
///   (`vc_transport_sticky`, `vc_transport_preference`, `vc_transport_session`)
///   so the next load resolves to the implicit default (`WebTransport`).
/// - **any value + sticky** (`(_, true)`): write `vc_transport_preference` +
///   `vc_transport_sticky=true` to `localStorage` so the choice persists across
///   browser sessions.
/// - **non-default + not sticky** (`(false, false)`): a session-scoped choice.
///   Clear any pre-existing `localStorage` sticky pin
///   (`vc_transport_sticky` + `vc_transport_preference`) FIRST, then write the
///   value to `vc_transport_session`. Clearing the stale sticky pin is required:
///   otherwise `load_transport_preference` would see `sticky == true` on the
///   next load, read the stale `localStorage` value, and ignore the
///   `sessionStorage` choice we just wrote (issue #1291 hazard C.3).
pub fn apply_transport_decision(pref: TransportPreference, sticky: bool) {
    let is_default = pref == TransportPreference::default();
    match (is_default, sticky) {
        // Default + not sticky: clear all storage — implicit default
        // doesn't need to be remembered.
        (true, false) => clear_transport_sticky_and_pref(),
        (_, true) => {
            save_transport_preference(pref);
            save_transport_sticky(true);
        }
        (false, false) => {
            // Session-scoped: survives the imminent reload but is forgotten on
            // tab close. Clear any prior localStorage sticky pin first so a
            // stale `vc_transport_sticky=true` can't shadow this session value
            // on the next load.
            if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten())
            {
                let _ = storage.remove_item(TRANSPORT_STICKY_KEY);
                let _ = storage.remove_item(TRANSPORT_PREF_KEY);
            }
            save_transport_preference_session(pref);
        }
    }
}

/// Handle a transport preference change from transport selection controls.
///
/// Shows a confirmation dialog. If the user confirms, persists the preference
/// via [`apply_transport_decision`] (sticky vs session-only depending on
/// `sticky`) and reloads the page. If cancelled, attempts to reset a native
/// `<select>` control (when present) back to the current value so it doesn't
/// appear stale.
///
/// Routing rules (delegated to [`apply_transport_decision`]):
///
/// - The default (`WebTransport`) selected with `sticky == false`: clear every
///   transport-preference storage key so the next load resolves to the default
///   without needing a remembered choice.
/// - Selecting any value with `sticky == true`: write to `localStorage` so the
///   choice persists across browser sessions.
/// - Non-default selection with `sticky == false`: clear any prior
///   `localStorage` sticky pin, then write to `sessionStorage` so the choice
///   survives the imminent page reload but evaporates when the tab closes. The
///   stale-sticky clear is what lets a session-scoped WebSocket choice win over
///   a previously pinned WebTransport on the next load (issue #1291).
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
        apply_transport_decision(pref, sticky);
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

/// Context for the user-imported custom theme (single slot).
///
/// `Some(name)` = custom theme active (name for display).
/// `None` = bundled default active.
#[derive(Clone, Copy)]
pub struct CustomThemeCtx(pub Signal<Option<String>>);

const THEME_STORAGE_KEY: &str = "ui-theme";

/// Load theme from localStorage; falls back to `Theme::Dark`.
pub fn load_theme_from_storage() -> Theme {
    read_local_storage(THEME_STORAGE_KEY)
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
    crate::theme_file::apply_theme_file_tokens(resolved);
}

/// Persist theme to localStorage and apply `data-theme` on `<html>`.
pub fn apply_and_save_theme(theme: Theme) {
    write_local_storage(THEME_STORAGE_KEY, theme.as_str());
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
    fn decode_budget_override_roundtrip_all() {
        // Issue #1466: `All` serializes to the independent literal "all" and
        // parses back. The literal is the external storage contract (the string
        // the persisted value must use), not derived from the enum — so a
        // mutation that emitted/parsed any other token breaks this.
        let serialized = serialize_decode_budget_override(DecodeBudgetOverride::All);
        assert_eq!(serialized, "all");
        assert_eq!(
            parse_decode_budget_override("all"),
            DecodeBudgetOverride::All
        );
    }

    #[test]
    fn decode_budget_override_all_is_distinct() {
        // `All` is its own variant: not Auto, not any Fixed(n). A mutation that
        // collapsed `All` into Auto or Fixed (e.g. parsing "all" => Auto) breaks
        // at least one of these.
        assert_ne!(DecodeBudgetOverride::All, DecodeBudgetOverride::Auto);
        assert_ne!(DecodeBudgetOverride::All, DecodeBudgetOverride::Fixed(6));
        assert_ne!(DecodeBudgetOverride::All, DecodeBudgetOverride::Fixed(1));
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
    fn restore_device_id_stored_wins_over_default_first_entry() {
        // Regression for the e2e restore-after-reload bug: Chrome's fake device
        // set lists a pseudo "default" mic FIRST. A persisted non-default
        // selection must win over that first "default" entry — restore must not
        // collapse to the auto-selected first device.
        let available = vec![
            "default".to_string(),
            "communications".to_string(),
            "real-mic-id".to_string(),
        ];
        assert_eq!(
            restore_device_id(Some("communications"), &available),
            Some("communications".to_string())
        );
        assert_eq!(
            restore_device_id(Some("real-mic-id"), &available),
            Some("real-mic-id".to_string())
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

    // ── GlowColor parser consolidation ──────────────────────────────────────
    // These pin the invariant that `from_rgb` is the single source of truth
    // for preset-vs-Custom disambiguation and that `from_hex` delegates to
    // `parse_hex` + `from_rgb` for the actual parsing. Reverting the
    // consolidation (e.g. re-inlining a duplicate preset table into
    // `from_hex`) is still allowed to pass these — the point is to catch
    // behavioural drift between the entry points.

    #[test]
    fn glow_color_from_rgb_detects_presets() {
        assert_eq!(GlowColor::from_rgb(255, 255, 255), GlowColor::White);
        assert_eq!(GlowColor::from_rgb(12, 175, 255), GlowColor::Cyan);
        assert_eq!(GlowColor::from_rgb(255, 0, 191), GlowColor::Magenta);
        assert_eq!(GlowColor::from_rgb(221, 160, 221), GlowColor::Plum);
        assert_eq!(GlowColor::from_rgb(91, 207, 159), GlowColor::MintGreen);
    }

    #[test]
    fn glow_color_from_rgb_falls_back_to_custom() {
        assert_eq!(
            GlowColor::from_rgb(0x12, 0x34, 0x56),
            GlowColor::Custom {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            }
        );
    }

    #[test]
    fn glow_color_from_hex_is_strict_and_matches_from_rgb() {
        // Strict format required by from_hex.
        // @token-exempt: hex literal is test input, not a rendered color
        assert_eq!(GlowColor::from_hex("#5bcf9f"), Some(GlowColor::MintGreen));
        assert_eq!(
            // @token-exempt: hex literal is test input, not a rendered color
            GlowColor::from_hex("#123456"),
            Some(GlowColor::Custom {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );

        // Anything not in `#RRGGBB` form is rejected — lenient callers must
        // go through `color_math::parse_hex` first, then `from_rgb`.
        // @token-exempt: hex literals are test inputs, not rendered colors
        assert_eq!(GlowColor::from_hex("5bcf9f"), None);
        // @token-exempt: hex literals are test inputs, not rendered colors
        assert_eq!(GlowColor::from_hex("#5bcf9"), None);
        assert_eq!(GlowColor::from_hex(""), None);
        // @token-exempt: hex literals are test inputs, not rendered colors
        assert_eq!(GlowColor::from_hex(" #5bcf9f "), None);

        // When the strict form parses, the result must match from_rgb —
        // otherwise the two entry points have drifted.
        // @token-exempt: hex literals are test inputs, not rendered colors
        for hex in ["#FFFFFF", "#0CAFFF", "#DDA0DD", "#5bcf9f", "#ABCDEF"] {
            let via_hex = GlowColor::from_hex(hex).unwrap();
            let (r, g, b) = crate::util::color_math::parse_hex(hex).unwrap();
            assert_eq!(via_hex, GlowColor::from_rgb(r, g, b));
        }
    }

    #[test]
    fn glow_color_from_storage_shares_parse_hex() {
        // The `custom:...` branch was previously an inline hex parser; it
        // now delegates to `color_math::parse_hex`. Verify both a canonical
        // form and a form that only `parse_hex`'s permissiveness accepts
        // (which storage never emits, but must not crash on).
        assert_eq!(
            GlowColor::from_storage("custom:123456"),
            Some(GlowColor::Custom {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );
        assert_eq!(GlowColor::from_storage("custom:zzzzzz"), None);
        // Preset names still round-trip.
        assert_eq!(
            GlowColor::from_storage("mint-green"),
            Some(GlowColor::MintGreen)
        );
    }
}
