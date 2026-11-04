use crate::local_storage::{load_json, remove_item, save_json};
use serde::{Deserialize, Serialize};

const STORAGE_KEY: &str = "vc_action_bar_layout";

/// Slots that the user must always be able to access. They never appear in
/// `hidden`, are never offered a remove button in the UI, and `Reset Buttons`
/// also resets `hidden` to empty so a user cannot wedge themselves mid-call by
/// removing their mute or camera-mute control.
pub const NON_REMOVABLE_SLOTS: &[ActionBarSlot] = &[ActionBarSlot::Mic, ActionBarSlot::Camera];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionBarSlot {
    Mic,
    Camera,
    Reactions,
    #[serde(rename = "raise_hand")]
    RaiseHand,
    #[serde(rename = "screen")]
    ScreenShare,
    #[serde(rename = "participants")]
    PeerList,
    #[serde(rename = "density")]
    DensityMode,
    Diagnostics,
    #[serde(rename = "settings")]
    DeviceSettings,
    #[serde(rename = "recording")]
    Recording,
    #[serde(rename = "meeting_options")]
    MeetingOptions,
    /// Issue 2136: the host's meeting-timer control. HOST-ONLY — see
    /// [`meeting_timer_slot_visible`].
    #[serde(rename = "meeting_timer")]
    MeetingTimer,
    Captions,
    #[serde(rename = "recent_actions")]
    RecentActions,
}

impl ActionBarSlot {
    /// Short human-readable label, used for the remove button's accessible name.
    pub fn display_name(self) -> &'static str {
        match self {
            ActionBarSlot::Mic => "Microphone",
            ActionBarSlot::Camera => "Camera",
            ActionBarSlot::Reactions => "Reactions",
            ActionBarSlot::RaiseHand => "Raise hand",
            ActionBarSlot::ScreenShare => "Screen share",
            ActionBarSlot::PeerList => "Participants",
            ActionBarSlot::DensityMode => "Density mode",
            ActionBarSlot::Diagnostics => "Diagnostics",
            ActionBarSlot::DeviceSettings => "Settings",
            ActionBarSlot::Recording => "Record",
            ActionBarSlot::MeetingOptions => "Meeting options",
            ActionBarSlot::MeetingTimer => "Meeting timer",
            ActionBarSlot::Captions => "Captions",
            ActionBarSlot::RecentActions => "Recent actions",
        }
    }

    /// What the control does, as the help page explains it.
    pub fn help_text(self) -> &'static str {
        match self {
            ActionBarSlot::Mic => "Mute or unmute your microphone. If it shows a \"!\", the microphone isn't available; select it to try again.",
            ActionBarSlot::Camera => "Turn your camera on or off. If it shows a \"!\", the camera isn't available; select it to try again.",
            ActionBarSlot::Reactions => "Send a reaction everyone in the call can see.",
            ActionBarSlot::RaiseHand => "Let everyone know you want to speak. Select it again to lower your hand.",
            ActionBarSlot::ScreenShare => "Show a window or your entire screen to everyone. While you share, select it again to share something else or stop. iPhone and iPad can't share their screen.",
            ActionBarSlot::PeerList => "See who's in the call. Search or sort the list, and use the pencil next to your name to rename yourself.",
            ActionBarSlot::DensityMode => "Choose how tightly participant tiles are packed on screen.",
            ActionBarSlot::Diagnostics => "See live connection quality, bitrate, packet loss and codec stats. It also holds the performance controls and the Show diagnostics on tiles option.",
            ActionBarSlot::DeviceSettings => "Open Settings to switch your microphone, camera or speaker and change how the app looks and behaves.",
            ActionBarSlot::Recording => "Record the call to a file on your device. Hosts get the Record button; other signed-in participants get it when a host allows recording for everyone. Guests can't use Record.",
            ActionBarSlot::MeetingOptions => "Hosts only. Change meeting options during the call: waiting room, who can admit people, guests and recording.",
            ActionBarSlot::MeetingTimer => "Hosts only. Show everyone a countdown, then add time or cancel it.",
            ActionBarSlot::Captions => "Show or hide live captions. A host turns captions on or off for everyone. Available when your organization has captions turned on; hidden while end-to-end encryption is on, or after the server refuses to turn captions on.",
            ActionBarSlot::RecentActions => "Jump back to features and settings you used recently. Choose how many to remember in Settings, under Preferences.",
        }
    }

    /// Whether the user is allowed to remove this slot from the action bar.
    /// Mic and Camera are pinned so a mid-call removal cannot leave the user
    /// without a mute / camera-mute control.
    pub fn is_removable(self) -> bool {
        !NON_REMOVABLE_SLOTS.contains(&self)
    }

    /// Stable, DOM-safe identifier used for the `data-slot` attribute on each
    /// action-bar wrapper. Matches the serde tag (v2 storage schema) so a
    /// single string round-trips through storage and keyboard-reorder lookups.
    pub fn slug(self) -> &'static str {
        match self {
            ActionBarSlot::Mic => "mic",
            ActionBarSlot::Camera => "camera",
            ActionBarSlot::Reactions => "reactions",
            ActionBarSlot::RaiseHand => "raise_hand",
            ActionBarSlot::ScreenShare => "screen",
            ActionBarSlot::PeerList => "participants",
            ActionBarSlot::DensityMode => "density",
            ActionBarSlot::Diagnostics => "diagnostics",
            ActionBarSlot::DeviceSettings => "settings",
            ActionBarSlot::Recording => "recording",
            ActionBarSlot::MeetingOptions => "meeting_options",
            ActionBarSlot::MeetingTimer => "meeting_timer",
            ActionBarSlot::Captions => "captions",
            ActionBarSlot::RecentActions => "recent_actions",
        }
    }

    /// Inverse of [`Self::slug`]. Used by the keyboard-reorder handler to map
    /// the `data-slot` attribute back to the enum variant.
    pub fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "mic" => ActionBarSlot::Mic,
            "camera" => ActionBarSlot::Camera,
            "reactions" => ActionBarSlot::Reactions,
            "raise_hand" => ActionBarSlot::RaiseHand,
            "screen" => ActionBarSlot::ScreenShare,
            "participants" => ActionBarSlot::PeerList,
            "density" => ActionBarSlot::DensityMode,
            "diagnostics" => ActionBarSlot::Diagnostics,
            "settings" => ActionBarSlot::DeviceSettings,
            "recording" => ActionBarSlot::Recording,
            "meeting_options" => ActionBarSlot::MeetingOptions,
            "meeting_timer" => ActionBarSlot::MeetingTimer,
            "captions" => ActionBarSlot::Captions,
            "recent_actions" => ActionBarSlot::RecentActions,
            _ => return None,
        })
    }
}

pub const DEFAULT_SLOTS: &[ActionBarSlot] = &[
    ActionBarSlot::Mic,
    ActionBarSlot::Camera,
    // Issue #1884: Reactions at index 2 (after Camera). Existing users get it
    // appended by migrate_stored_layout's forward-compat pass.
    ActionBarSlot::Reactions,
    // Issue 2135: Raise hand at index 3, immediately after Reactions — the two
    // are the same class of action (a broadcast signal to the room) and users
    // reach for them interchangeably. Existing users get it appended by
    // `migrate_stored_layout`'s forward-compat pass, so nobody's saved ordering
    // is rewritten; only a fresh install sees it here.
    ActionBarSlot::RaiseHand,
    ActionBarSlot::ScreenShare,
    ActionBarSlot::PeerList,
    ActionBarSlot::DensityMode,
    ActionBarSlot::Diagnostics,
    ActionBarSlot::DeviceSettings,
    ActionBarSlot::Recording,
    ActionBarSlot::MeetingOptions,
    // Issue 2136: the meeting timer sits next to Meeting options — both are
    // host-only surfaces, so a non-host sees neither and the visible bar is
    // unchanged for them. Existing users get it appended by
    // `migrate_stored_layout`'s forward-compat pass.
    ActionBarSlot::MeetingTimer,
    ActionBarSlot::RecentActions,
];

macro_rules! all_slots {
    ($($slot:ident),+ $(,)?) => {
        /// Every [`ActionBarSlot`].
        pub const ALL_SLOTS: &[ActionBarSlot] = &[$(ActionBarSlot::$slot),+];
        const _: fn(ActionBarSlot) = |slot| match slot {
            $(ActionBarSlot::$slot)|+ => {}
        };
    };
}

all_slots!(
    Mic,
    Camera,
    Reactions,
    RaiseHand,
    ScreenShare,
    PeerList,
    DensityMode,
    Diagnostics,
    DeviceSettings,
    Recording,
    MeetingOptions,
    MeetingTimer,
    Captions,
    RecentActions,
);

/// [`DEFAULT_SLOTS`], plus the Captions slot when the captions flag is on, so a
/// flag-off build never writes Captions into a stored layout.
pub fn default_slots(captions_enabled: bool) -> Vec<ActionBarSlot> {
    let mut slots = DEFAULT_SLOTS.to_vec();
    if captions_enabled {
        slots.insert(slots.len() - 1, ActionBarSlot::Captions);
    }
    slots
}

/// On-disk schema for the action-bar layout. Two shapes have ever shipped to
/// users:
///
/// * **v1 (legacy, pre-#1278 remove feature)** — a bare JSON array of slot
///   tags. There was no remove UI, so nothing was ever intentionally hidden.
/// * **v2 (current)** — an object that records BOTH the user's visible
///   ordering AND the slots they explicitly removed. Storing `hidden` is what
///   lets the loader distinguish "user removed this slot" from "this slot
///   didn't exist when the layout was saved" — the latter still gets appended
///   to the bar (forward-compat for newly-added defaults), the former does
///   NOT come back on reload.
///
/// This struct is **write-side only**. The loader parses v2 input
/// element-by-element from a raw `serde_json::Value` (see
/// `migrate_stored_layout`) rather than whole-struct-deserializing through
/// this type — that lets a single unknown / future-renamed slot tag be
/// dropped instead of taking the whole layout down with it.
#[derive(Debug, Clone, Serialize)]
struct StoredLayoutV2 {
    #[serde(rename = "v")]
    version: u32,
    slots: Vec<ActionBarSlot>,
    hidden: Vec<ActionBarSlot>,
}

const SCHEMA_VERSION: u32 = 2;

/// Result of running the storage-migration pipeline against a parsed
/// `serde_json::Value`. The third tuple element flags whether the loader
/// should persist the migrated layout back to storage as v2 so subsequent
/// loads take the v2 fast path.
struct Migrated {
    slots: Vec<ActionBarSlot>,
    hidden: Vec<ActionBarSlot>,
    needs_persist: bool,
}

/// Pure migration: turn a raw stored value (v1 array, v2 object, or junk)
/// into the canonical `(visible_slots, hidden_slots)` pair, applying the
/// forward-compat (append-missing-default) and Mic/Camera-pinning rules.
///
/// This is the function that BOTH `load_action_bar_layout` and the unit
/// tests call — so a regression in this logic is caught by the tests
/// without them re-implementing the migration inline (the "test pins what
/// it names" rule from CLAUDE.md).
fn migrate_stored_layout(raw: serde_json::Value, defaults: &[ActionBarSlot]) -> Migrated {
    let (mut slots, hidden, came_from_v1) = match &raw {
        // v2 object form. Parse `slots` and `hidden` element-by-element so a
        // single unknown / future-renamed slot tag in either array is dropped
        // rather than aborting the whole struct parse (a whole-struct
        // `from_value::<StoredLayoutV2>` would fall into the `Err` arm on one
        // bad tag and resurrect every removed slot via the default fallback —
        // the exact bug-class this loader exists to prevent). Mirrors the
        // element-by-element parse the v1 path below uses.
        serde_json::Value::Object(map) => {
            let parse_slot_array = |key: &str| -> Vec<ActionBarSlot> {
                map.get(key)
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                serde_json::from_value::<ActionBarSlot>(item.clone()).ok()
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            (parse_slot_array("slots"), parse_slot_array("hidden"), false)
        }
        // v1 legacy array. The remove feature did not ship in v1, so anything
        // missing from the saved array was missing because the slot did not
        // exist yet, NOT because the user removed it. Treat hidden as empty
        // and append missing defaults below.
        serde_json::Value::Array(_) => {
            let arr =
                serde_json::from_value::<Vec<serde_json::Value>>(raw.clone()).unwrap_or_default();
            let parsed: Vec<ActionBarSlot> = arr
                .into_iter()
                .filter_map(|v| serde_json::from_value::<ActionBarSlot>(v).ok())
                .collect();
            (parsed, Vec::new(), true)
        }
        // No saved value (fresh install) or an unrecognised shape. Fall back to
        // the full default ordering with nothing hidden.
        _ => (defaults.to_vec(), Vec::new(), false),
    };

    // Forward-compat: append defaults the user has neither chosen NOR hidden.
    // Critically, defaults present in `hidden` are NOT appended — that is
    // exactly the resurrect-on-reload bug we are fixing.
    let mut appended_default = false;
    for default in defaults {
        if !slots.contains(default) && !hidden.contains(default) {
            slots.push(*default);
            appended_default = true;
        }
    }

    // Non-removable slots can never sit in `hidden`. If a previously-allowed
    // removal of Mic/Camera made it into v2 storage, scrub it now AND make
    // sure the slot is visible.
    let hidden: Vec<ActionBarSlot> = hidden.into_iter().filter(|s| s.is_removable()).collect();
    let mut scrubbed_non_removable = false;
    for pinned in NON_REMOVABLE_SLOTS {
        if !slots.contains(pinned) {
            slots.push(*pinned);
            scrubbed_non_removable = true;
        }
    }
    // Reasonable position: Mic/Camera should sit at the front if we had to
    // re-insert them. Keep the rest of the order intact.
    if scrubbed_non_removable {
        let mut reordered = Vec::with_capacity(slots.len());
        for pinned in NON_REMOVABLE_SLOTS {
            if slots.contains(pinned) {
                reordered.push(*pinned);
            }
        }
        for s in &slots {
            if !reordered.contains(s) {
                reordered.push(*s);
            }
        }
        slots = reordered;
    }

    Migrated {
        slots,
        hidden,
        needs_persist: came_from_v1 || appended_default || scrubbed_non_removable,
    }
}

/// Returns `(visible_ordered_slots, removed_slots)` for the current user.
///
/// Forward-compat rule (the only "auto-appears" path): a [`default_slots`] slot
/// that is present in NEITHER `slots` NOR `hidden` is appended to `slots` (and
/// the result is saved as v2). A slot that lives in `hidden` is never
/// resurrected by this function — that is what fixes the #1278 "removed widget
/// re-appears on reload" regression.
pub fn load_action_bar_layout(captions_enabled: bool) -> (Vec<ActionBarSlot>, Vec<ActionBarSlot>) {
    // Parse the raw storage value once. `serde_json::Value::Null` is a safe
    // fallback because it triggers the "missing / unrecognised" branch inside
    // `migrate_stored_layout` (initial `default_slots`, empty hidden).
    let raw: serde_json::Value = load_json(STORAGE_KEY, serde_json::Value::Null);
    let Migrated {
        slots,
        hidden,
        needs_persist,
    } = migrate_stored_layout(raw, &default_slots(captions_enabled));

    if needs_persist {
        save_json(
            STORAGE_KEY,
            &StoredLayoutV2 {
                version: SCHEMA_VERSION,
                slots: slots.clone(),
                hidden: hidden.clone(),
            },
        );
    }

    (slots, hidden)
}

/// Build the canonical v2 layout that `save_action_bar_layout` will write to
/// `localStorage`. Factored out so unit tests can pin the exact production
/// filtering (non-removable slots dropped from `hidden`) without having to
/// drive `web_sys::window()` from a native `cargo test` run.
fn build_stored_for_save(slots: &[ActionBarSlot], hidden: &[ActionBarSlot]) -> StoredLayoutV2 {
    StoredLayoutV2 {
        version: SCHEMA_VERSION,
        slots: slots.to_vec(),
        // Defensive: never persist a non-removable slot into `hidden`, even if
        // a caller passes one in by mistake.
        hidden: hidden
            .iter()
            .copied()
            .filter(|s| s.is_removable())
            .collect(),
    }
}

pub fn save_action_bar_layout(slots: &[ActionBarSlot], hidden: &[ActionBarSlot]) {
    save_json(STORAGE_KEY, &build_stored_for_save(slots, hidden));
}

pub fn remove_action_bar_layout() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(STORAGE_KEY);
    }
}

pub const DOCK_AUTOHIDE_HINT: &str = "Fades out when idle";
pub const DOCK_AUTOSHRINK_HINT: &str = "Fewer buttons until you need them";

const SHRUNK_STORAGE_KEY: &str = "vc_action_bar_shrunk";

/// The buttons that stay on the action bar while it is shrunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShrunkButtons {
    pub slots: Vec<ActionBarSlot>,
    pub hangup: bool,
}

impl Default for ShrunkButtons {
    fn default() -> Self {
        Self {
            slots: vec![
                ActionBarSlot::Mic,
                ActionBarSlot::Camera,
                ActionBarSlot::ScreenShare,
            ],
            hangup: true,
        }
    }
}

impl ShrunkButtons {
    pub fn keeps(&self, slot: ActionBarSlot) -> bool {
        self.slots.contains(&slot)
    }

    pub fn with_slot(&self, slot: ActionBarSlot, keep: bool) -> Self {
        let mut next = self.clone();
        next.slots.retain(|s| *s != slot);
        if keep {
            next.slots.push(slot);
        }
        next
    }
}

fn parse_shrunk_buttons(raw: &serde_json::Value) -> ShrunkButtons {
    let (Some(slots), Some(hangup)) = (
        raw.get("slots").and_then(|v| v.as_array()),
        raw.get("hangup").and_then(|v| v.as_bool()),
    ) else {
        return ShrunkButtons::default();
    };
    ShrunkButtons {
        slots: slots
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
        hangup,
    }
}

pub fn load_shrunk_buttons() -> ShrunkButtons {
    parse_shrunk_buttons(&load_json(SHRUNK_STORAGE_KEY, serde_json::Value::Null))
}

pub fn save_shrunk_buttons(kept: &ShrunkButtons) {
    save_json(SHRUNK_STORAGE_KEY, kept);
}

pub fn remove_shrunk_buttons() {
    remove_item(SHRUNK_STORAGE_KEY);
}

pub fn stays_on_shrunk_bar(kept: &ShrunkButtons, slot: ActionBarSlot) -> bool {
    kept.keeps(slot)
}

/// True when Hang up is not kept and nothing that stays is on the bar.
pub fn no_kept_button_on_screen(
    kept: &ShrunkButtons,
    visible: &[ActionBarSlot],
    overflowed: &[ActionBarSlot],
) -> bool {
    !kept.hangup
        && !visible
            .iter()
            .any(|slot| stays_on_shrunk_bar(kept, *slot) && !overflowed.contains(slot))
}

/// `secondary` reordered for the overflow fit, so slots that stay on the shrunk
/// bar overflow last. Stable within each group.
pub fn kept_slots_first(secondary: &[ActionBarSlot], kept: &ShrunkButtons) -> Vec<ActionBarSlot> {
    let (first, rest): (Vec<ActionBarSlot>, Vec<ActionBarSlot>) = secondary
        .iter()
        .partition(|slot| stays_on_shrunk_bar(kept, **slot));
    [first, rest].concat()
}

/// Whether the Recording action-bar slot should render for the current user.
///
/// Recording is restricted by three conditions:
///   1. Guests (unauthenticated users) NEVER see the record button, regardless
///      of any other setting.
///   2. The host ALWAYS sees the record button (when not a guest).
///   3. Authenticated non-host participants see the button only when the
///      meeting's `recording_allowed_for_all` setting is enabled.
///
/// Extracted as a pure function so the gating decision is host-testable
/// without a Dioxus runtime; the production render path calls this directly.
pub fn record_slot_visible(
    is_owner: bool,
    is_guest: bool,
    recording_allowed_for_all: bool,
) -> bool {
    !is_guest && (is_owner || recording_allowed_for_all)
}

/// Whether the meeting-timer slot is visible for this user (issue 2136).
///
/// HOST-ONLY, and `is_host` here MUST be the caller's LIVE host signal — the one
/// kept in step by the HOST_GRANTED / HOST_REVOKED broadcasts — not the `is_owner`
/// prop, which only catches up after a `check_status` round-trip. The relay
/// authorizes MEETING_TIMER against its own live mirror of the same flag, so a
/// gate on the lagging value shows a demoted ex-host controls whose every packet
/// is silently dropped.
///
/// Guests are excluded outright: a guest is never the host, so the term is
/// redundant against a correct `is_host`, but it is stated explicitly here for
/// the same reason `record_slot_visible` states it — the two flags being mutually
/// exclusive is a property of how tokens are minted, not an invariant anything
/// enforces.
///
/// Extracted as a pure function so the gating decision is host-testable without a
/// Dioxus runtime; the production render path calls this directly.
pub fn meeting_timer_slot_visible(is_host: bool, is_guest: bool) -> bool {
    !is_guest && is_host
}

/// Result of a keyboard-reorder attempt on the action-bar. `old_idx` is the
/// slot's position before the keystroke; `new_idx` is its position after.
/// When they are equal the caller MUST NOT persist and should still announce
/// a "no-op" to the aria-live region (the slot is already at that edge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardReorderResult {
    pub old_idx: usize,
    pub new_idx: usize,
}

/// Apply a keyboard reorder (Arrow / Home / End) to `slots` in-place.
///
/// `delta` is a relative step (`-1` for Left/Up, `+1` for Right/Down) when
/// arrow keys are pressed. `absolute` overrides `delta` (`Some(0)` = Home,
/// `Some(i32::MAX)` = End). Values are clamped to the current list bounds so
/// the caller does not need to bound-check.
///
/// Returns `None` if `slot` is not present in `slots` OR if `slots` is empty.
/// Otherwise returns `Some(KeyboardReorderResult { old_idx, new_idx })`; the
/// slot is moved in the vector when `new_idx != old_idx`.
///
/// The keyboard-reorder handler in `attendants.rs` calls this function
/// directly, so a mutation to the clamp or index math is observable in the
/// unit tests below (see [`tests::keyboard_reorder_*`]).
pub fn apply_keyboard_reorder(
    slots: &mut Vec<ActionBarSlot>,
    slot: ActionBarSlot,
    delta: Option<i32>,
    absolute: Option<i32>,
) -> Option<KeyboardReorderResult> {
    if slots.is_empty() {
        return None;
    }
    let old_idx = slots.iter().position(|s| *s == slot)?;
    let max_idx = (slots.len() - 1) as i32;
    let new_idx = if let Some(d) = delta {
        (old_idx as i32 + d).clamp(0, max_idx) as usize
    } else {
        absolute.unwrap_or(0).clamp(0, max_idx) as usize
    };
    if new_idx != old_idx {
        slots.remove(old_idx);
        slots.insert(new_idx, slot);
    }
    Some(KeyboardReorderResult { old_idx, new_idx })
}

pub const HOLD_TO_EDIT_MS: u32 = 2_000;
pub const HOLD_SLOP_PX: f64 = 10.0;
pub const HOLD_EDIT_IDLE_MS: u32 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomizeEntry {
    Menu,
    Hold(ActionBarSlot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomizeExit {
    Done,
    Escape,
    BandTooNarrow,
    Idle,
}

pub fn customize_entry_announcement(entry: CustomizeEntry) -> &'static str {
    match entry {
        CustomizeEntry::Hold(_) => {
            "Editing action bar buttons. Drag to move one. Press Done or Escape to finish."
        }
        CustomizeEntry::Menu => {
            "Editing action bar buttons. Use arrow keys to move the focused button, or drag with the pointer. Press the minus button to hide one. Press Done or Escape to finish."
        }
    }
}

pub fn customize_exit_announcement(reason: CustomizeExit) -> &'static str {
    match reason {
        CustomizeExit::Idle => "Finished editing action bar buttons.",
        CustomizeExit::Done | CustomizeExit::Escape | CustomizeExit::BandTooNarrow => "",
    }
}

pub fn customize_session_is_timed(entry: CustomizeEntry) -> bool {
    matches!(entry, CustomizeEntry::Hold(_))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldPress {
    pub is_primary: bool,
    pub button: i16,
    pub ctrl_key: bool,
    pub on_slot_primary_button: bool,
    pub slot_menu_open: bool,
}

pub fn hold_should_arm(edit_fits: bool, press: HoldPress) -> bool {
    edit_fits
        && press.is_primary
        && press.button == 0
        && !press.ctrl_key
        && press.on_slot_primary_button
        && !press.slot_menu_open
}

pub fn hold_exceeds_slop(down: (f64, f64), now: (f64, f64)) -> bool {
    let dx = now.0 - down.0;
    let dy = now.1 - down.1;
    dx * dx + dy * dy > HOLD_SLOP_PX * HOLD_SLOP_PX
}

pub fn hold_should_fire(
    customize_mode: bool,
    edit_fits: bool,
    slot: ActionBarSlot,
    rendered: &[ActionBarSlot],
    overflowed: &[ActionBarSlot],
) -> bool {
    !customize_mode && edit_fits && rendered.contains(&slot) && !overflowed.contains(&slot)
}

pub fn drag_grab_offset(pointer: (f64, f64), origin: (f64, f64), size: (f64, f64)) -> (f64, f64) {
    let dx = pointer.0 - origin.0;
    let dy = pointer.1 - origin.1;
    if (0.0..=size.0).contains(&dx) && (0.0..=size.1).contains(&dy) {
        (dx, dy)
    } else {
        (size.0 / 2.0, size.1 / 2.0)
    }
}

pub fn press_pointer(down: &mut Vec<i32>, pointer_id: i32) {
    if !down.contains(&pointer_id) {
        down.push(pointer_id);
    }
}

pub fn release_pointer(down: &mut Vec<i32>, pointer_id: i32) -> bool {
    let before = down.len();
    down.retain(|id| *id != pointer_id);
    down.len() != before
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleStep {
    Exit,
    Wait(u32),
}

pub fn idle_step(now_ms: f64, last_activity_ms: f64, pointer_down: bool) -> IdleStep {
    let window = f64::from(HOLD_EDIT_IDLE_MS);
    if pointer_down {
        return IdleStep::Wait(HOLD_EDIT_IDLE_MS);
    }
    let remaining = last_activity_ms + window - now_ms;
    if remaining <= 0.0 {
        IdleStep::Exit
    } else {
        IdleStep::Wait(remaining.ceil().min(window) as u32)
    }
}

pub fn hangup_press_is_swallowed(
    pointer_down_ms: Option<f64>,
    click_ms: f64,
    from_pointer: bool,
) -> bool {
    from_pointer
        && pointer_down_ms.is_some_and(|down| click_ms - down >= f64::from(HOLD_TO_EDIT_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // Issue 2136 -- the meeting-timer slot gate
    // ---------------------------------------------------------------------

    /// The gate must be a real DISCRIMINATOR in both directions. A gate that
    /// returned a constant would still let the control render (or never render),
    /// and the failure is silent either way: a non-host who sees the control gets
    /// buttons the relay drops on the floor.
    #[test]
    fn meeting_timer_slot_is_visible_only_to_a_non_guest_host() {
        assert!(
            meeting_timer_slot_visible(true, false),
            "the host must see the meeting-timer control"
        );
        assert!(
            !meeting_timer_slot_visible(false, false),
            "a non-host must NOT see it -- the relay drops their packets, so the \
             control would look live and silently do nothing"
        );
    }

    /// Guests are excluded regardless of the host flag. This is belt-and-braces
    /// (a guest is never the host), and it is written down for the same reason
    /// `record_slot_visible` states it: the two flags being mutually exclusive is
    /// a property of how tokens are minted, not an invariant anything enforces.
    #[test]
    fn meeting_timer_slot_is_hidden_from_guests_even_if_the_host_flag_is_set() {
        assert!(!meeting_timer_slot_visible(true, true));
        assert!(!meeting_timer_slot_visible(false, true));
    }

    /// The slot must be a real member of the default layout, and must round-trip
    /// through the persistence tag. A slot whose tag did not parse back would be
    /// silently dropped from a saved layout on the next load.
    #[test]
    fn meeting_timer_slot_round_trips_through_its_storage_tag() {
        assert!(
            DEFAULT_SLOTS.contains(&ActionBarSlot::MeetingTimer),
            "the meeting timer must be in the default layout or a fresh install \
             never shows it"
        );
        let tag = ActionBarSlot::MeetingTimer.slug();
        assert_eq!(
            ActionBarSlot::from_slug(tag),
            Some(ActionBarSlot::MeetingTimer),
            "slug/from_slug must round-trip, or a saved layout and the \
             keyboard-reorder lookup both lose the slot"
        );
        assert_eq!(
            tag, "meeting_timer",
            "the slug is also the serde tag and the `data-slot` attribute the \
             E2E spec addresses; changing it silently drops the slot from every \
             already-saved layout"
        );
    }

    #[test]
    fn all_slots_lists_every_slot_once() {
        for slot in ALL_SLOTS.iter().copied() {
            assert_eq!(ActionBarSlot::from_slug(slot.slug()), Some(slot));
            assert_eq!(
                ALL_SLOTS.iter().filter(|s| **s == slot).count(),
                1,
                "{slot:?}"
            );
        }
    }

    #[test]
    fn captions_slot_round_trips_through_its_storage_tag() {
        assert_eq!(ActionBarSlot::Captions.slug(), "captions");
        assert_eq!(
            ActionBarSlot::from_slug("captions"),
            Some(ActionBarSlot::Captions)
        );
        assert_eq!(
            serde_json::to_value(ActionBarSlot::Captions).unwrap(),
            serde_json::json!("captions")
        );
    }

    /// Test wrapper: calls the EXACT same migration the production loader
    /// runs, but without touching `localStorage`. Because this delegates to
    /// `migrate_stored_layout` (the function `load_action_bar_layout` itself
    /// uses), a regression in the production migration is observable here.
    /// The tests do NOT re-implement the migration inline.
    fn migrate_for_test(raw: serde_json::Value) -> (Vec<ActionBarSlot>, Vec<ActionBarSlot>) {
        let Migrated { slots, hidden, .. } = migrate_stored_layout(raw, DEFAULT_SLOTS);
        (slots, hidden)
    }

    #[test]
    fn the_captions_slot_is_only_a_default_while_the_flag_is_on() {
        assert!(!DEFAULT_SLOTS.contains(&ActionBarSlot::Captions));
        assert!(!default_slots(false).contains(&ActionBarSlot::Captions));
        assert!(
            default_slots(true).ends_with(&[ActionBarSlot::Captions, ActionBarSlot::RecentActions])
        );

        let stored = serde_json::to_value(build_stored_for_save(DEFAULT_SLOTS, &[])).unwrap();
        let off = migrate_stored_layout(stored.clone(), &default_slots(false));
        assert!(!off.needs_persist, "flag off must not rewrite the layout");
        assert!(!off.slots.contains(&ActionBarSlot::Captions));

        let on = migrate_stored_layout(stored, &default_slots(true));
        assert!(on.needs_persist);
        assert_eq!(on.slots.last(), Some(&ActionBarSlot::Captions));

        let fresh = migrate_stored_layout(serde_json::Value::Null, &default_slots(false));
        assert!(!fresh.slots.contains(&ActionBarSlot::Captions));
    }

    /// Regression for #1278 follow-up: a slot removed in v2 storage must NOT
    /// reappear on the next load. This is the test that FAILS on the
    /// unfixed loader (which appended every missing default and resurrected
    /// the removed slot).
    #[test]
    fn removed_slot_stays_removed_after_reload() {
        // Simulate: user has removed `PeerList` and `Diagnostics`. v2 storage
        // omits them from `slots` and records them in `hidden`.
        let mut slots: Vec<ActionBarSlot> = DEFAULT_SLOTS
            .iter()
            .copied()
            .filter(|s| *s != ActionBarSlot::PeerList && *s != ActionBarSlot::Diagnostics)
            .collect();
        // Move things around a bit so we also verify ordering is preserved.
        slots.swap(2, 3);
        let hidden = vec![ActionBarSlot::PeerList, ActionBarSlot::Diagnostics];

        let raw = serde_json::json!({
            "v": 2,
            "slots": slots,
            "hidden": hidden,
        });

        let (loaded_slots, loaded_hidden) = migrate_for_test(raw);

        assert!(
            !loaded_slots.contains(&ActionBarSlot::PeerList),
            "PeerList must stay removed after reload; got slots={loaded_slots:?}"
        );
        assert!(
            !loaded_slots.contains(&ActionBarSlot::Diagnostics),
            "Diagnostics must stay removed after reload; got slots={loaded_slots:?}"
        );
        assert!(loaded_hidden.contains(&ActionBarSlot::PeerList));
        assert!(loaded_hidden.contains(&ActionBarSlot::Diagnostics));
        assert_eq!(loaded_slots, slots, "visible slot order must round-trip");
    }

    /// Forward-compat: a v2 layout that pre-dates a newly-added default slot
    /// must auto-append the new default (the bug-free half of the original
    /// migration). Combined with `removed_slot_stays_removed_after_reload`,
    /// this is the load contract.
    #[test]
    fn unknown_default_gets_appended_on_load() {
        // Simulate a saved v2 layout that doesn't yet know about `MeetingOptions`.
        let saved: Vec<ActionBarSlot> = DEFAULT_SLOTS
            .iter()
            .copied()
            .filter(|s| *s != ActionBarSlot::MeetingOptions)
            .collect();
        let raw = serde_json::json!({
            "v": 2,
            "slots": saved,
            "hidden": [],
        });

        let (loaded_slots, loaded_hidden) = migrate_for_test(raw);
        assert!(
            loaded_slots.contains(&ActionBarSlot::MeetingOptions),
            "missing default must be appended"
        );
        assert!(loaded_hidden.is_empty());
    }

    /// v1 legacy bare-array storage must migrate to v2 without losing the
    /// user's ordering and without inventing a `hidden` list (the remove
    /// feature did not exist in v1, so nothing was ever intentionally hidden).
    #[test]
    fn v1_legacy_array_migrates_with_empty_hidden() {
        let raw = serde_json::json!(["camera", "mic", "settings"]);
        let (loaded_slots, loaded_hidden) = migrate_for_test(raw);
        assert_eq!(loaded_slots[0], ActionBarSlot::Camera);
        assert_eq!(loaded_slots[1], ActionBarSlot::Mic);
        assert_eq!(loaded_slots[2], ActionBarSlot::DeviceSettings);
        // All other defaults appended.
        assert!(loaded_slots.contains(&ActionBarSlot::PeerList));
        assert!(loaded_hidden.is_empty());
    }

    /// Forward-version-skew regression: a v2 layout written by a newer client
    /// (a slot tag that this build does not know about) must NOT take down
    /// the whole parse. Known slots and removals must survive; unknown tags
    /// are dropped from BOTH `slots` and `hidden`. Pre-fix, the v2 path used
    /// whole-struct `serde_json::from_value::<StoredLayoutV2>`, which fails
    /// on a single unknown tag and falls into the default-resurrect arm —
    /// every removed slot would come back and the user's order would be
    /// lost. This is the test that FAILS on that un-fixed loader.
    #[test]
    fn v2_with_unknown_tags_drops_unknowns_and_preserves_known() {
        // Build a layout where:
        //  - `slots` mixes known tags with one unknown future tag.
        //  - `hidden` mixes a known removal with one unknown tag.
        //  - `PeerList` is in `hidden` (the user removed it).
        // If the parse is whole-struct, ALL of `slots` AND `hidden` are
        // discarded and the loader returns DEFAULT_SLOTS with empty hidden —
        // i.e. PeerList resurrects and the custom order is gone.
        let raw = serde_json::json!({
            "v": 2,
            "slots": [
                "camera",
                "mic",
                "future_widget_xyz",
                "settings",
                "screen",
            ],
            "hidden": ["participants", "another_unknown_tag"],
        });

        let (loaded_slots, loaded_hidden) = migrate_for_test(raw);

        // Known slots survived in their saved order (Mic/Camera pinned first
        // by the non-removable pass; the rest keep their order).
        assert_eq!(loaded_slots[0], ActionBarSlot::Camera);
        assert_eq!(loaded_slots[1], ActionBarSlot::Mic);
        assert!(loaded_slots.contains(&ActionBarSlot::DeviceSettings));
        assert!(loaded_slots.contains(&ActionBarSlot::ScreenShare));

        // The user's removal of PeerList must survive the parse.
        assert!(
            !loaded_slots.contains(&ActionBarSlot::PeerList),
            "PeerList must stay removed across an unknown-tag parse; got slots={loaded_slots:?}"
        );
        assert!(
            loaded_hidden.contains(&ActionBarSlot::PeerList),
            "PeerList must remain in hidden across an unknown-tag parse; got hidden={loaded_hidden:?}"
        );

        // No unknown tag leaked into the typed Vecs (they have no variant for
        // it, so this is structurally enforced; assert against the count to
        // catch any future loosening).
        assert_eq!(loaded_hidden.len(), 1);
    }

    /// Non-removable slots must never be honored as `hidden`, even if a buggy
    /// older build wrote them there. The loader must restore them to `slots`.
    #[test]
    fn non_removable_slot_in_hidden_is_scrubbed() {
        let raw = serde_json::json!({
            "v": 2,
            "slots": ["participants", "screen"],
            "hidden": ["mic", "camera"],
        });
        let (loaded_slots, loaded_hidden) = migrate_for_test(raw);
        assert!(loaded_slots.contains(&ActionBarSlot::Mic));
        assert!(loaded_slots.contains(&ActionBarSlot::Camera));
        assert!(!loaded_hidden.contains(&ActionBarSlot::Mic));
        assert!(!loaded_hidden.contains(&ActionBarSlot::Camera));
    }

    /// `save_action_bar_layout` strips non-removable slots from `hidden`
    /// defensively — a future caller bug that pushes Mic/Camera into the
    /// hidden list must not poison storage. Drives the **production**
    /// `build_stored_for_save` helper so deleting the filter from the
    /// production code path would FAIL this test.
    #[test]
    fn save_strips_non_removable_from_hidden() {
        let stored = build_stored_for_save(
            &[ActionBarSlot::PeerList, ActionBarSlot::ScreenShare],
            &[
                ActionBarSlot::Mic,
                ActionBarSlot::Camera,
                ActionBarSlot::Diagnostics,
            ],
        );
        assert_eq!(stored.version, SCHEMA_VERSION);
        assert!(!stored.hidden.contains(&ActionBarSlot::Mic));
        assert!(!stored.hidden.contains(&ActionBarSlot::Camera));
        // Removable entries pass through unchanged.
        assert_eq!(stored.hidden, vec![ActionBarSlot::Diagnostics]);
        // `slots` is written verbatim (no filter applied there).
        assert_eq!(
            stored.slots,
            vec![ActionBarSlot::PeerList, ActionBarSlot::ScreenShare]
        );
    }

    // ── record_slot_visible ──────────────────────────────────────────────
    //
    // These tests guard the visibility rule for the Recording slot: hosts
    // always see it, non-hosts only when the setting `recording_allowed_for_all`
    // is on.  Each test would FAIL if the predicate were mutated:
    //   • Return `is_owner && ...` → non-host with setting ON hides ← test 3
    //   • Return `... && recording_allowed_for_all` → host with setting OFF hides ← test 1
    //   • Return `true` → non-host with setting OFF sees it ← test 4
    //   • Return `false` → host is never shown ← test 1

    #[test]
    fn record_slot_visible_for_host_setting_off() {
        assert!(
            record_slot_visible(true, false, false),
            "host must always see record button regardless of the setting"
        );
    }

    #[test]
    fn record_slot_visible_for_host_setting_on() {
        assert!(
            record_slot_visible(true, false, true),
            "host sees record button"
        );
    }

    #[test]
    fn record_slot_visible_for_non_host_setting_on() {
        assert!(
            record_slot_visible(false, false, true),
            "non-host sees record button when the setting is on"
        );
    }

    #[test]
    fn record_slot_hidden_for_non_host_setting_off() {
        assert!(
            !record_slot_visible(false, false, false),
            "non-host must NOT see record button when the setting is off (default)"
        );
    }

    #[test]
    fn record_slot_hidden_for_guest_even_when_setting_on() {
        assert!(
            !record_slot_visible(false, true, true),
            "guest must NEVER see record button, even when recording_allowed_for_all is enabled"
        );
    }

    #[test]
    fn record_slot_hidden_for_guest_when_setting_off() {
        assert!(
            !record_slot_visible(false, true, false),
            "guest must NEVER see record button when setting is off"
        );
    }

    #[test]
    fn record_slot_hidden_for_guest_who_is_host() {
        assert!(
            !record_slot_visible(true, true, false),
            "guest status overrides host status - guests do not see the record button"
        );
    }

    /// Round-trip proof: `from_slug(slug(x)) == Some(x)` for every enum variant.
    /// Mutation check: swap any pair of arms in either `slug` or `from_slug`
    /// and this test fails. Also asserts the slugs are the same DOM-safe
    /// tokens used by the v2 storage schema (see the `#[serde]` renames), so
    /// changing one side without the other is caught.
    #[test]
    fn slug_roundtrip_for_every_variant() {
        for slot in DEFAULT_SLOTS.iter().copied() {
            let s = slot.slug();
            assert_eq!(
                ActionBarSlot::from_slug(s),
                Some(slot),
                "slug round-trip failed for {slot:?} (slug={s:?})",
            );
            // No whitespace, matches CSS attribute-selector safety.
            assert!(!s.contains(' '), "slug {s:?} must be whitespace-free");
            assert!(!s.is_empty(), "slug for {slot:?} must be non-empty");
        }
        // Unknown slug → None.
        assert_eq!(ActionBarSlot::from_slug("bogus"), None);
        assert_eq!(ActionBarSlot::from_slug(""), None);
    }

    /// Arrow-key delta moves the slot by exactly one position and clamps at
    /// the edges. Mutation check: replace `clamp(0, max_idx)` with a
    /// wrapping/unchecked expression and this test fails.
    #[test]
    fn keyboard_reorder_arrow_delta_moves_and_clamps() {
        // Right arrow from the middle. ScreenShare sits at index 4 in
        // DEFAULT_SLOTS (after Mic, Camera, Reactions, RaiseHand — issue #1884
        // inserted Reactions at index 2 and issue 2135 inserted RaiseHand at
        // index 3), so a +1 arrow moves it to index 5. The index is read from
        // DEFAULT_SLOTS rather than hard-coded so a future insertion does not
        // silently make this test assert about a different slot.
        let mut slots: Vec<ActionBarSlot> = DEFAULT_SLOTS.to_vec();
        let screen_idx = DEFAULT_SLOTS
            .iter()
            .position(|s| *s == ActionBarSlot::ScreenShare)
            .expect("ScreenShare is a default slot");
        assert_eq!(
            screen_idx, 4,
            "ScreenShare's default position (Mic, Camera, Reactions, RaiseHand first)"
        );
        let result = apply_keyboard_reorder(&mut slots, ActionBarSlot::ScreenShare, Some(1), None)
            .expect("ScreenShare is present");
        assert_eq!(result.old_idx, screen_idx);
        assert_eq!(result.new_idx, screen_idx + 1);
        assert_eq!(slots[screen_idx + 1], ActionBarSlot::ScreenShare);

        // Left arrow at index 0 clamps (no move).
        let mut slots: Vec<ActionBarSlot> = DEFAULT_SLOTS.to_vec();
        let result = apply_keyboard_reorder(&mut slots, ActionBarSlot::Mic, Some(-1), None)
            .expect("Mic is present");
        assert_eq!(result.old_idx, 0);
        assert_eq!(result.new_idx, 0);
        assert_eq!(slots, DEFAULT_SLOTS.to_vec());

        // Right arrow at the last index clamps.
        let mut slots: Vec<ActionBarSlot> = DEFAULT_SLOTS.to_vec();
        let last = *DEFAULT_SLOTS.last().unwrap();
        let last_idx = DEFAULT_SLOTS.len() - 1;
        let result = apply_keyboard_reorder(&mut slots, last, Some(1), None).expect("present");
        assert_eq!(result.old_idx, last_idx);
        assert_eq!(result.new_idx, last_idx);
    }

    /// Home jumps to 0, End jumps to len-1 (clamped from `i32::MAX`).
    /// Mutation check: swap the branches or drop the `absolute` clamp and
    /// this test fails.
    #[test]
    fn keyboard_reorder_home_end_absolute_jumps() {
        let mut slots: Vec<ActionBarSlot> = DEFAULT_SLOTS.to_vec();
        let last = *DEFAULT_SLOTS.last().unwrap();
        let len = DEFAULT_SLOTS.len();

        // Home: move last slot to index 0.
        let result = apply_keyboard_reorder(&mut slots, last, None, Some(0)).expect("present");
        assert_eq!(result.new_idx, 0);
        assert_eq!(slots[0], last);

        // End: move the (now index-0) slot back to the last index. `i32::MAX`
        // must clamp to `len-1` without overflow.
        let result =
            apply_keyboard_reorder(&mut slots, last, None, Some(i32::MAX)).expect("present");
        assert_eq!(result.new_idx, len - 1);
        assert_eq!(slots[len - 1], last);
    }

    /// Absent slot returns None; empty list returns None. Both are guards
    /// the keyboard handler relies on for early-return.
    #[test]
    fn keyboard_reorder_returns_none_for_absent_or_empty() {
        let mut slots: Vec<ActionBarSlot> = vec![ActionBarSlot::Mic, ActionBarSlot::Camera];
        assert_eq!(
            apply_keyboard_reorder(&mut slots, ActionBarSlot::ScreenShare, Some(1), None),
            None,
        );
        // Original list is untouched.
        assert_eq!(slots, vec![ActionBarSlot::Mic, ActionBarSlot::Camera]);

        let mut empty: Vec<ActionBarSlot> = Vec::new();
        assert_eq!(
            apply_keyboard_reorder(&mut empty, ActionBarSlot::Mic, Some(1), None),
            None,
        );
    }

    fn press() -> HoldPress {
        HoldPress {
            is_primary: true,
            button: 0,
            ctrl_key: false,
            on_slot_primary_button: true,
            slot_menu_open: false,
        }
    }

    #[test]
    fn hold_arms_only_for_a_plain_primary_press_on_a_slot_button() {
        assert!(hold_should_arm(true, press()));
        assert!(
            !hold_should_arm(false, press()),
            "a band too narrow for edit mode must not arm a hold"
        );
        for rejected in [
            HoldPress {
                is_primary: false,
                ..press()
            },
            HoldPress {
                button: 2,
                ..press()
            },
            HoldPress {
                button: 1,
                ..press()
            },
            HoldPress {
                ctrl_key: true,
                ..press()
            },
            HoldPress {
                on_slot_primary_button: false,
                ..press()
            },
            HoldPress {
                slot_menu_open: true,
                ..press()
            },
        ] {
            assert!(!hold_should_arm(true, rejected), "{rejected:?}");
        }
    }

    #[test]
    fn hold_slop_is_ten_px_straight_line() {
        let down = (100.0, 100.0);
        assert!(!hold_exceeds_slop(down, down));
        assert!(!hold_exceeds_slop(down, (110.0, 100.0)));
        assert!(
            !hold_exceeds_slop(down, (106.0, 108.0)),
            "exactly 10px straight-line (14px Manhattan) must not cancel"
        );
        assert!(hold_exceeds_slop(down, (106.0, 108.01)));
        assert!(hold_exceeds_slop(down, (100.0, 89.5)));
        assert!(!hold_exceeds_slop(down, (104.0, 104.0)));
        assert!(hold_exceeds_slop(down, (107.1, 107.1)));
    }

    #[test]
    fn hold_fires_only_while_the_slot_is_still_reachable() {
        let rendered = [ActionBarSlot::Mic, ActionBarSlot::PeerList];
        let fire = |customize, fits, slot, overflowed: &[ActionBarSlot]| {
            hold_should_fire(customize, fits, slot, &rendered, overflowed)
        };
        assert!(fire(false, true, ActionBarSlot::PeerList, &[]));
        assert!(
            !fire(false, false, ActionBarSlot::PeerList, &[]),
            "a band that narrowed mid-hold must not enter edit mode"
        );
        assert!(!fire(true, true, ActionBarSlot::PeerList, &[]));
        assert!(!fire(false, true, ActionBarSlot::Reactions, &[]));
        assert!(!fire(
            false,
            true,
            ActionBarSlot::PeerList,
            &[ActionBarSlot::PeerList]
        ));
        assert!(fire(
            false,
            true,
            ActionBarSlot::PeerList,
            &[ActionBarSlot::Reactions]
        ));
    }

    #[test]
    fn lifting_one_finger_keeps_the_other_finger_down() {
        let mut down = Vec::new();
        press_pointer(&mut down, 1);
        press_pointer(&mut down, 2);
        press_pointer(&mut down, 2);
        assert_eq!(down, vec![1, 2], "a repeated press is tracked once");
        assert!(release_pointer(&mut down, 2));
        assert_eq!(down, vec![1], "the first finger is still down");
        assert!(
            !release_pointer(&mut down, 7),
            "an unknown pointer releases nothing"
        );
        assert!(release_pointer(&mut down, 1));
        assert!(down.is_empty());
    }

    #[test]
    fn drag_grab_keeps_the_offset_inside_the_slot_and_centres_outside_it() {
        let origin = (100.0, 50.0);
        let size = (40.0, 40.0);
        assert_eq!(drag_grab_offset((110.0, 60.0), origin, size), (10.0, 10.0));
        assert_eq!(drag_grab_offset((140.0, 90.0), origin, size), (40.0, 40.0));
        assert_eq!(drag_grab_offset((100.0, 50.0), origin, size), (0.0, 0.0));
        assert_eq!(
            drag_grab_offset((174.0, 60.0), origin, size),
            (20.0, 20.0),
            "a slot that slid away from the finger must centre the preview on it"
        );
        assert_eq!(drag_grab_offset((110.0, 49.0), origin, size), (20.0, 20.0));
        assert_eq!(drag_grab_offset((99.0, 60.0), origin, size), (20.0, 20.0));
    }

    #[test]
    fn idle_step_exits_only_after_five_quiet_seconds_with_no_pointer_down() {
        assert_eq!(idle_step(10_000.0, 5_000.0, false), IdleStep::Exit);
        assert_eq!(idle_step(10_001.0, 5_000.0, false), IdleStep::Exit);
        assert_eq!(idle_step(9_999.0, 5_000.0, false), IdleStep::Wait(1));
        assert_eq!(idle_step(5_000.0, 5_000.0, false), IdleStep::Wait(5_000));
        assert_eq!(idle_step(7_500.4, 5_000.0, false), IdleStep::Wait(2_500));
        assert_eq!(idle_step(60_000.0, 5_000.0, true), IdleStep::Wait(5_000));
        assert_eq!(
            idle_step(1_000.0, 5_000.0, false),
            IdleStep::Wait(5_000),
            "a clock that steps backwards waits at most one window"
        );
    }

    #[test]
    fn hangup_swallows_only_a_pointer_press_held_for_the_hold_time() {
        let hold = f64::from(HOLD_TO_EDIT_MS);
        assert!(hangup_press_is_swallowed(
            Some(1_000.0),
            1_000.0 + hold,
            true
        ));
        assert!(!hangup_press_is_swallowed(
            Some(1_000.0),
            1_000.0 + hold - 1.0,
            true
        ));
        assert!(!hangup_press_is_swallowed(
            Some(1_000.0),
            1_000.0 + hold,
            false
        ));
        assert!(!hangup_press_is_swallowed(None, 1_000.0 + hold, true));
    }

    #[test]
    fn only_hold_entry_is_timed_and_only_idle_exit_speaks() {
        assert!(customize_session_is_timed(CustomizeEntry::Hold(
            ActionBarSlot::Mic
        )));
        assert!(!customize_session_is_timed(CustomizeEntry::Menu));
        assert_eq!(
            customize_exit_announcement(CustomizeExit::Idle),
            "Finished editing action bar buttons."
        );
        for silent in [
            CustomizeExit::Done,
            CustomizeExit::Escape,
            CustomizeExit::BandTooNarrow,
        ] {
            assert_eq!(customize_exit_announcement(silent), "");
        }
        assert_eq!(
            customize_entry_announcement(CustomizeEntry::Hold(ActionBarSlot::Mic)),
            "Editing action bar buttons. Drag to move one. Press Done or Escape to finish."
        );
    }

    fn stored(kept: &ShrunkButtons) -> serde_json::Value {
        serde_json::to_value(kept).expect("kept buttons serialize")
    }

    #[test]
    fn shrunk_buttons_round_trip_through_storage_shape() {
        let kept = ShrunkButtons {
            slots: vec![ActionBarSlot::RaiseHand, ActionBarSlot::PeerList],
            hangup: false,
        };
        assert_eq!(parse_shrunk_buttons(&stored(&kept)), kept);
        assert_eq!(
            parse_shrunk_buttons(&stored(&ShrunkButtons::default())),
            ShrunkButtons::default()
        );
    }

    #[test]
    fn an_empty_selection_stays_empty_instead_of_returning_to_the_default() {
        let empty = ShrunkButtons {
            slots: Vec::new(),
            hangup: false,
        };
        assert_eq!(parse_shrunk_buttons(&stored(&empty)), empty);
    }

    #[test]
    fn one_unknown_stored_tag_is_dropped_and_the_rest_survive() {
        let raw = serde_json::json!({ "slots": ["participants", "from_the_future", "mic"], "hangup": false });
        assert_eq!(
            parse_shrunk_buttons(&raw),
            ShrunkButtons {
                slots: vec![ActionBarSlot::PeerList, ActionBarSlot::Mic],
                hangup: false,
            }
        );
    }

    #[test]
    fn missing_or_junk_storage_falls_back_to_the_default_kept_set() {
        for raw in [
            serde_json::Value::Null,
            serde_json::json!("junk"),
            serde_json::json!([]),
            serde_json::json!({ "slots": ["mic"] }),
            serde_json::json!({ "hangup": false }),
            serde_json::json!({ "slots": "mic", "hangup": false }),
            serde_json::json!({ "slots": ["mic"], "hangup": "no" }),
        ] {
            assert_eq!(
                parse_shrunk_buttons(&raw),
                ShrunkButtons::default(),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_default_kept_set_is_camera_mic_share_and_hang_up() {
        let kept = ShrunkButtons::default();
        assert!(kept.hangup);
        for slot in ALL_SLOTS.iter().copied() {
            let expected = matches!(
                slot,
                ActionBarSlot::Mic | ActionBarSlot::Camera | ActionBarSlot::ScreenShare
            );
            assert_eq!(kept.keeps(slot), expected, "{slot:?}");
        }
    }

    #[test]
    fn with_slot_adds_and_removes_one_slot_without_duplicates() {
        let kept = ShrunkButtons::default();
        let without = kept.with_slot(ActionBarSlot::Mic, false);
        assert!(!without.keeps(ActionBarSlot::Mic));
        assert!(without.keeps(ActionBarSlot::Camera));
        let again = without
            .with_slot(ActionBarSlot::PeerList, true)
            .with_slot(ActionBarSlot::PeerList, true);
        assert_eq!(
            again
                .slots
                .iter()
                .filter(|s| **s == ActionBarSlot::PeerList)
                .count(),
            1
        );
        assert_eq!(again.hangup, kept.hangup);
    }

    fn kept_set(slots: &[ActionBarSlot], hangup: bool) -> ShrunkButtons {
        ShrunkButtons {
            slots: slots.to_vec(),
            hangup,
        }
    }

    #[test]
    fn no_kept_button_on_screen_covers_hang_up_visibility_and_overflow() {
        let mic = ActionBarSlot::Mic;
        let share = ActionBarSlot::ScreenShare;
        let on_screen =
            |kept: &ShrunkButtons, visible: &[ActionBarSlot], over: &[ActionBarSlot]| {
                !no_kept_button_on_screen(kept, visible, over)
            };

        assert!(!on_screen(&kept_set(&[], false), &[mic], &[]));
        assert!(on_screen(&kept_set(&[], true), &[mic], &[]));
        assert!(on_screen(&kept_set(&[mic], false), &[mic], &[]));
        assert!(
            !on_screen(&kept_set(&[share], false), &[mic], &[]),
            "a kept slot this user cannot see, such as Share on iOS, is off the bar"
        );
        assert!(
            !on_screen(&kept_set(&[share], false), &[mic, share], &[share]),
            "a kept slot in the overflow menu is off the bar"
        );
        assert!(on_screen(
            &kept_set(&[mic, share], false),
            &[mic, share],
            &[share]
        ));
    }

    #[test]
    fn only_the_kept_choice_keeps_a_slot_on_the_shrunk_bar() {
        let none_kept = kept_set(&[], false);
        for slot in ALL_SLOTS.iter().copied() {
            assert!(
                !stays_on_shrunk_bar(&none_kept, slot),
                "{slot:?} was not selected"
            );
        }
        assert!(stays_on_shrunk_bar(
            &kept_set(&[ActionBarSlot::PeerList], false),
            ActionBarSlot::PeerList,
        ));
    }

    #[test]
    fn an_unselected_mic_and_camera_leave_a_bare_bar() {
        let visible = [ActionBarSlot::Mic, ActionBarSlot::Camera];
        assert!(no_kept_button_on_screen(
            &kept_set(&[], false),
            &visible,
            &[],
        ));
    }

    #[test]
    fn slots_that_stay_come_first_for_the_fit_and_keep_their_order() {
        use ActionBarSlot::{Diagnostics, PeerList, Reactions, ScreenShare};
        let secondary = [Reactions, PeerList, ScreenShare, Diagnostics];
        let kept = kept_set(&[ScreenShare], true);
        assert_eq!(
            kept_slots_first(&secondary, &kept),
            [ScreenShare, Reactions, PeerList, Diagnostics]
        );
    }
}
