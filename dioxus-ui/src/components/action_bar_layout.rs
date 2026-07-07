use crate::local_storage::{load_json, save_json};
use serde::{Deserialize, Serialize};

const STORAGE_KEY: &str = "vc_action_bar_layout";

/// Slots that the user must always be able to access. They never appear in
/// `hidden`, are never offered a remove button in the UI, and `Reset to Default`
/// also resets `hidden` to empty so a user cannot wedge themselves mid-call by
/// removing their mute or camera-mute control.
pub const NON_REMOVABLE_SLOTS: &[ActionBarSlot] = &[ActionBarSlot::Mic, ActionBarSlot::Camera];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionBarSlot {
    Mic,
    Camera,
    #[serde(rename = "screen")]
    ScreenShare,
    #[serde(rename = "participants")]
    PeerList,
    #[serde(rename = "density")]
    DensityMode,
    Diagnostics,
    #[serde(rename = "settings")]
    DeviceSettings,
    #[serde(rename = "meeting_options")]
    MeetingOptions,
}

impl ActionBarSlot {
    /// Short human-readable label, used for the remove button's accessible name.
    pub fn display_name(self) -> &'static str {
        match self {
            ActionBarSlot::Mic => "Microphone",
            ActionBarSlot::Camera => "Camera",
            ActionBarSlot::ScreenShare => "Screen share",
            ActionBarSlot::PeerList => "Participants",
            ActionBarSlot::DensityMode => "Density mode",
            ActionBarSlot::Diagnostics => "Diagnostics",
            ActionBarSlot::DeviceSettings => "Settings",
            ActionBarSlot::MeetingOptions => "Meeting options",
        }
    }

    /// Whether the user is allowed to remove this slot from the action bar.
    /// Mic and Camera are pinned so a mid-call removal cannot leave the user
    /// without a mute / camera-mute control.
    pub fn is_removable(self) -> bool {
        !NON_REMOVABLE_SLOTS.contains(&self)
    }
}

pub const DEFAULT_SLOTS: &[ActionBarSlot] = &[
    ActionBarSlot::Mic,
    ActionBarSlot::Camera,
    ActionBarSlot::ScreenShare,
    ActionBarSlot::PeerList,
    ActionBarSlot::DensityMode,
    ActionBarSlot::Diagnostics,
    ActionBarSlot::DeviceSettings,
    ActionBarSlot::MeetingOptions,
];

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
fn migrate_stored_layout(raw: serde_json::Value) -> Migrated {
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
        _ => (DEFAULT_SLOTS.to_vec(), Vec::new(), false),
    };

    // Forward-compat: append defaults the user has neither chosen NOR hidden.
    // Critically, defaults present in `hidden` are NOT appended — that is
    // exactly the resurrect-on-reload bug we are fixing.
    let mut appended_default = false;
    for default in DEFAULT_SLOTS {
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
/// Forward-compat rule (the only "auto-appears" path): a `DEFAULT_SLOTS` slot
/// that is present in NEITHER `slots` NOR `hidden` is appended to `slots` (and
/// the result is saved as v2). A slot that lives in `hidden` is never
/// resurrected by this function — that is what fixes the #1278 "removed widget
/// re-appears on reload" regression.
pub fn load_action_bar_layout() -> (Vec<ActionBarSlot>, Vec<ActionBarSlot>) {
    // Parse the raw storage value once. `serde_json::Value::Null` is a safe
    // fallback because it triggers the "missing / unrecognised" branch inside
    // `migrate_stored_layout` (initial DEFAULT_SLOTS, empty hidden).
    let raw: serde_json::Value = load_json(STORAGE_KEY, serde_json::Value::Null);
    let Migrated {
        slots,
        hidden,
        needs_persist,
    } = migrate_stored_layout(raw);

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

pub fn slot_index(slots: &[ActionBarSlot], slot: ActionBarSlot) -> Option<usize> {
    slots.iter().position(|s| *s == slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test wrapper: calls the EXACT same migration the production loader
    /// runs, but without touching `localStorage`. Because this delegates to
    /// `migrate_stored_layout` (the function `load_action_bar_layout` itself
    /// uses), a regression in the production migration is observable here.
    /// The tests do NOT re-implement the migration inline.
    fn migrate_for_test(raw: serde_json::Value) -> (Vec<ActionBarSlot>, Vec<ActionBarSlot>) {
        let Migrated { slots, hidden, .. } = migrate_stored_layout(raw);
        (slots, hidden)
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
        assert!(loaded_slots.contains(&ActionBarSlot::ScreenShare));
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
}
