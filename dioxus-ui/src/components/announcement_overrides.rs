/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Meeting-wide overrides of each participant's join/leave announcement
//! preferences.

use crate::components::attendants::try_set_if_changed;
use crate::meeting_api::{JoinMeetingResponse, MeetingInfo};
use dioxus::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
pub use videocall_meeting_types::AnnouncementOverride;

/// One value per join/leave announcement channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerAnnouncement<T> {
    pub entry_message: T,
    pub entry_sound: T,
    pub exit_message: T,
    pub exit_sound: T,
}

/// The meeting's four overrides, as the server last reported them.
pub type AnnouncementOverrides = PerAnnouncement<AnnouncementOverride>;

/// A PATCH of the overrides; `None` leaves that field as the server has it.
pub type OverridePatch = PerAnnouncement<Option<AnnouncementOverride>>;

/// The live overrides of the meeting the user is in.
#[derive(Clone, Copy)]
pub struct AnnouncementOverridesCtx(pub Signal<AnnouncementOverrides>);

/// Per meeting, the override save chains that ended after their editor's owner
/// unmounted and that no page has refreshed for yet.
#[derive(Clone, Copy)]
pub struct OrphanedOverrideSavesCtx(pub Signal<HashMap<String, u32>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverrideField {
    EntryMessage,
    EntrySound,
    ExitMessage,
    ExitSound,
}

impl OverrideField {
    pub const ALL: [Self; 4] = [
        Self::EntryMessage,
        Self::EntrySound,
        Self::ExitMessage,
        Self::ExitSound,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::EntryMessage => "Join message",
            Self::EntrySound => "Join sound",
            Self::ExitMessage => "Leave message",
            Self::ExitSound => "Leave sound",
        }
    }

    pub const fn testid(self) -> &'static str {
        match self {
            Self::EntryMessage => "meeting-option-join-message",
            Self::EntrySound => "meeting-option-join-sound",
            Self::ExitMessage => "meeting-option-leave-message",
            Self::ExitSound => "meeting-option-leave-sound",
        }
    }

    pub const fn channel(self) -> &'static str {
        match self {
            Self::EntryMessage | Self::ExitMessage => "Message",
            Self::EntrySound | Self::ExitSound => "Sound",
        }
    }

    const fn plural(self) -> &'static str {
        match self {
            Self::EntryMessage => "Join messages",
            Self::EntrySound => "Join sounds",
            Self::ExitMessage => "Leave messages",
            Self::ExitSound => "Leave sounds",
        }
    }
}

impl<T: Copy> PerAnnouncement<T> {
    pub fn get(&self, field: OverrideField) -> T {
        match field {
            OverrideField::EntryMessage => self.entry_message,
            OverrideField::EntrySound => self.entry_sound,
            OverrideField::ExitMessage => self.exit_message,
            OverrideField::ExitSound => self.exit_sound,
        }
    }

    pub fn set(&mut self, field: OverrideField, value: T) {
        match field {
            OverrideField::EntryMessage => self.entry_message = value,
            OverrideField::EntrySound => self.entry_sound = value,
            OverrideField::ExitMessage => self.exit_message = value,
            OverrideField::ExitSound => self.exit_sound = value,
        }
    }
}

impl AnnouncementOverrides {
    pub fn from_status(status: &JoinMeetingResponse) -> Self {
        Self {
            entry_message: status.entry_message_override,
            entry_sound: status.entry_sound_override,
            exit_message: status.exit_message_override,
            exit_sound: status.exit_sound_override,
        }
    }

    pub fn from_meeting_info(info: &MeetingInfo) -> Self {
        Self {
            entry_message: info.entry_message_override,
            entry_sound: info.entry_sound_override,
            exit_message: info.exit_message_override,
            exit_sound: info.exit_sound_override,
        }
    }
}

pub const fn option_label(value: AnnouncementOverride) -> &'static str {
    match value {
        AnnouncementOverride::Participant => "Their choice",
        AnnouncementOverride::On => "On for everyone",
        AnnouncementOverride::Off => "Off for everyone",
    }
}

/// `(show_toast, play_sound)` for one join or leave event. Recording forces the
/// toast, even when the host turned messages off, but never the sound.
pub fn announcement_decision(
    message_override: AnnouncementOverride,
    sound_override: AnnouncementOverride,
    show_pref: bool,
    sound_pref: bool,
    recording_active: bool,
) -> (bool, bool) {
    (
        message_override.resolve(show_pref) || recording_active,
        sound_override.resolve(sound_pref),
    )
}

/// What a screen reader is told when the host changes the overrides.
pub fn override_change_announcement(
    prev: &AnnouncementOverrides,
    next: &AnnouncementOverrides,
) -> Option<String> {
    let lines: Vec<String> = OverrideField::ALL
        .into_iter()
        .filter(|&field| prev.get(field) != next.get(field))
        .map(|field| {
            let what = field.plural();
            match next.get(field) {
                AnnouncementOverride::On => {
                    format!("The host turned {} on for everyone.", what.to_lowercase())
                }
                AnnouncementOverride::Off => {
                    format!("The host turned {} off for everyone.", what.to_lowercase())
                }
                AnnouncementOverride::Participant => format!("{what} are your choice again."),
            }
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join(" "))
}

/// The host's unconfirmed override choices, saved with at most one PATCH in
/// flight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverrideSaver {
    chosen: OverridePatch,
    in_flight: Option<OverridePatch>,
    unsettled: u32,
}

impl OverrideSaver {
    /// The value the table shows: the unconfirmed choice, else `server`'s.
    pub fn shown(
        &self,
        field: OverrideField,
        server: &AnnouncementOverrides,
    ) -> AnnouncementOverride {
        self.chosen.get(field).unwrap_or(server.get(field))
    }

    /// Records a choice the table does not already show and returns its pick
    /// number; nothing is sent until [`Self::settled`] gets the latest one.
    pub fn choose(
        &mut self,
        field: OverrideField,
        value: AnnouncementOverride,
        server: &AnnouncementOverrides,
    ) -> Option<u32> {
        if self.shown(field, server) == value {
            return None;
        }
        let saved =
            self.in_flight.and_then(|sent| sent.get(field)).is_none() && server.get(field) == value;
        self.chosen.set(field, (!saved).then_some(value));
        self.unsettled += 1;
        Some(self.unsettled)
    }

    /// Pick `pick` stood unchanged; returns the PATCH to send if it was the
    /// latest and nothing is in flight.
    pub fn settled(&mut self, pick: u32) -> Option<OverridePatch> {
        if pick != self.unsettled {
            return None;
        }
        self.unsettled = 0;
        self.next_patch()
    }

    /// Settles the latest choice now instead of waiting.
    pub fn flush(&mut self) -> Option<OverridePatch> {
        self.settled(self.unsettled)
    }

    /// The in-flight PATCH was saved; returns the next one, if a settled choice
    /// made meanwhile is still unsaved.
    pub fn succeeded(&mut self) -> Option<OverridePatch> {
        if let Some(sent) = self.in_flight.take() {
            for field in OverrideField::ALL {
                if sent.get(field).is_some() && self.chosen.get(field) == sent.get(field) {
                    self.chosen.set(field, None);
                }
            }
        }
        self.next_patch()
    }

    /// The in-flight PATCH failed: drops every unconfirmed choice, so each row
    /// shows the server's value again, and returns their fields.
    pub fn failed(&mut self) -> Vec<OverrideField> {
        let reverted = OverrideField::ALL
            .into_iter()
            .filter(|&field| self.chosen.get(field).is_some())
            .collect();
        *self = Self {
            unsettled: self.unsettled,
            ..Self::default()
        };
        reverted
    }

    fn next_patch(&mut self) -> Option<OverridePatch> {
        if self.unsettled != 0
            || self.in_flight.is_some()
            || self.chosen == OverridePatch::default()
        {
            return None;
        }
        self.in_flight = Some(self.chosen);
        self.in_flight
    }
}

/// The overrides the meeting-options controls edit, with the host's unsaved
/// choices and the last save error.
#[derive(Clone, Copy, PartialEq)]
pub struct OverrideEditor {
    pub server: Signal<AnnouncementOverrides>,
    pub saver: Signal<OverrideSaver>,
    pub error: Signal<Option<String>>,
    reads: CopyValue<OverrideReads>,
    handoff: CopyValue<SaverHandoff>,
    orphaned: Option<Signal<HashMap<String, u32>>>,
}

/// The saver as its owner left it on unmount.
pub type SaverHandoff = Rc<Cell<Option<OverrideSaver>>>;

/// Reads of `server` (a settings re-fetch, or a save's response), numbered when
/// each is sent.
#[derive(Clone, Copy, Debug, Default)]
struct OverrideReads {
    sent: u64,
    landed: u64,
}

impl OverrideEditor {
    /// Applies `edit` to the saver, writing it back only if it changed.
    pub fn edit_saver<R>(&self, edit: impl FnOnce(&mut OverrideSaver) -> R) -> Option<R> {
        let mut saver = self.saver;
        let before = *saver.try_peek().ok()?;
        let mut after = before;
        let out = edit(&mut after);
        if after != before {
            *saver.try_write().ok()? = after;
        }
        Some(out)
    }

    /// Numbers a read of the server's overrides; take it before sending.
    pub fn start_read(&self) -> u64 {
        let mut reads = self.reads;
        reads
            .try_write()
            .map(|mut r| {
                r.sent += 1;
                r.sent
            })
            .unwrap_or(0)
    }

    /// Writes a read's `value` to `server`, unless a read sent after it landed.
    pub fn land_read(&self, read: u64, value: AnnouncementOverrides) {
        let mut reads = self.reads;
        let Ok(mut r) = reads.try_write() else {
            return;
        };
        if read <= r.landed {
            return;
        }
        r.landed = read;
        drop(r);
        try_set_if_changed(self.server, value);
    }

    /// Where the owner leaves its saver when it unmounts.
    pub fn handoff(&self) -> SaverHandoff {
        self.handoff
            .try_peek()
            .map(|h| Rc::clone(&h))
            .unwrap_or_default()
    }

    /// Counts a save chain for `meeting_id` that ended after this editor's owner
    /// unmounted.
    pub fn count_orphaned_save(&self, meeting_id: &str) {
        if let Some(mut saves) = self.orphaned {
            if let Ok(mut saves) = saves.try_write() {
                *saves.entry(meeting_id.to_owned()).or_default() += 1;
            }
        }
    }
}

/// Whenever `meeting_id` has counted save chains, on mount or when one is
/// counted, removes its entry and calls `refresh` once.
pub fn use_orphaned_save_refresh(meeting_id: &str, mut refresh: impl FnMut() + 'static) {
    let saves = try_use_context::<OrphanedOverrideSavesCtx>();
    let meeting_id = meeting_id.to_owned();
    use_effect(move || {
        let Some(OrphanedOverrideSavesCtx(mut saves)) = saves else {
            return;
        };
        if !saves.read().contains_key(&meeting_id) {
            return;
        }
        if let Ok(mut saves) = saves.try_write() {
            saves.remove(&meeting_id);
        }
        refresh();
    });
}

pub fn use_override_editor(server: Signal<AnnouncementOverrides>) -> OverrideEditor {
    let saver = use_signal(OverrideSaver::default);
    let handoff = use_hook(|| CopyValue::new(SaverHandoff::default()));
    use_drop(move || {
        if let (Ok(saver), Ok(handoff)) = (saver.try_peek(), handoff.try_peek()) {
            handoff.set(Some(*saver));
        }
    });
    OverrideEditor {
        server,
        saver,
        error: use_signal(|| None),
        reads: use_hook(|| CopyValue::new(OverrideReads::default())),
        handoff,
        orphaned: try_use_context::<OrphanedOverrideSavesCtx>().map(|ctx| ctx.0),
    }
}

pub fn override_save_error(reverted: &[OverrideField], server: &AnnouncementOverrides) -> String {
    reverted
        .iter()
        .map(|&field| {
            format!(
                "Couldn't save {}. It's back to \"{}\".",
                field.label(),
                option_label(server.get(field))
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use AnnouncementOverride::{Off, On, Participant};
    use OverrideField::{EntryMessage, EntrySound, ExitMessage, ExitSound};

    #[test]
    fn the_decision_covers_every_override_preference_and_recording_state() {
        // (override, own message pref, recording) -> (toast, sound); the sound
        // pref is always the opposite of the message pref so a swap shows.
        let table = [
            (Participant, true, false, (true, false)),
            (Participant, true, true, (true, false)),
            (Participant, false, false, (false, true)),
            (Participant, false, true, (true, true)),
            (On, true, false, (true, true)),
            (On, true, true, (true, true)),
            (On, false, false, (true, true)),
            (On, false, true, (true, true)),
            (Off, true, false, (false, false)),
            (Off, true, true, (true, false)),
            (Off, false, false, (false, false)),
            (Off, false, true, (true, false)),
        ];
        for (ov, show_pref, recording, want) in table {
            assert_eq!(
                announcement_decision(ov, ov, show_pref, !show_pref, recording),
                want,
                "override {ov:?}, message pref {show_pref}, recording {recording}"
            );
        }
    }

    #[test]
    fn the_message_and_sound_overrides_are_independent() {
        assert_eq!(
            announcement_decision(On, Off, false, true, false),
            (true, false)
        );
        assert_eq!(
            announcement_decision(Off, On, true, false, false),
            (false, true)
        );
    }

    fn server(entry_message: AnnouncementOverride) -> AnnouncementOverrides {
        AnnouncementOverrides {
            entry_message,
            ..Default::default()
        }
    }

    fn patch(field: OverrideField, value: AnnouncementOverride) -> OverridePatch {
        let mut patch = OverridePatch::default();
        patch.set(field, Some(value));
        patch
    }

    fn sent(saver: &mut OverrideSaver, field: OverrideField, value: AnnouncementOverride) {
        let pick = saver.choose(field, value, &AnnouncementOverrides::default());
        assert_eq!(saver.settled(pick.unwrap()), Some(patch(field, value)));
    }

    #[test]
    fn a_choice_shows_at_once_and_is_sent_once_it_settles() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        let pick = saver.choose(EntryMessage, On, &none).unwrap();
        assert_eq!(saver.shown(EntryMessage, &none), On);
        assert_eq!(saver.succeeded(), None, "nothing is sent before it settles");
        assert_eq!(saver.settled(pick), Some(patch(EntryMessage, On)));
        assert_eq!(saver.succeeded(), None);
        assert_eq!(saver, OverrideSaver::default());
        assert_eq!(saver.shown(EntryMessage, &server(Off)), Off);
    }

    #[test]
    fn a_sweep_sends_only_its_last_choice() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        let first = saver.choose(EntryMessage, On, &none).unwrap();
        let last = saver.choose(EntryMessage, Off, &none).unwrap();
        assert_eq!(
            saver.settled(first),
            None,
            "a newer choice restarts the wait"
        );
        assert_eq!(saver.shown(EntryMessage, &none), Off);
        assert_eq!(saver.settled(last), Some(patch(EntryMessage, Off)));
    }

    #[test]
    fn a_flush_sends_the_settling_choice_at_once_and_only_once() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        assert_eq!(saver.flush(), None);
        assert_eq!(saver, OverrideSaver::default());
        saver.choose(EntryMessage, On, &none).unwrap();
        let last = saver.choose(EntryMessage, Off, &none).unwrap();
        assert_eq!(saver.flush(), Some(patch(EntryMessage, Off)));
        assert_eq!(saver.settled(last), None, "its wait sends nothing more");
        assert_eq!(saver.flush(), None);
        let during = saver.choose(ExitSound, On, &none).unwrap();
        assert_eq!(saver.flush(), None, "one PATCH in flight");
        assert_eq!(saver.settled(during), None);
        assert_eq!(saver.succeeded(), Some(patch(ExitSound, On)));
        assert_eq!(saver.succeeded(), None);
        assert_eq!(saver, OverrideSaver::default());
    }

    #[test]
    fn a_choice_the_table_already_shows_records_nothing() {
        let mut saver = OverrideSaver::default();
        assert_eq!(saver.choose(EntryMessage, Off, &server(Off)), None);
        assert_eq!(saver, OverrideSaver::default());
        let pick = saver.choose(EntryMessage, On, &server(Off)).unwrap();
        assert_eq!(saver.choose(EntryMessage, On, &server(Off)), None);
        assert_eq!(saver.settled(pick), Some(patch(EntryMessage, On)));
    }

    #[test]
    fn sweeping_back_to_the_saved_value_sends_nothing() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        saver.choose(EntryMessage, On, &none).unwrap();
        let back = saver.choose(EntryMessage, Participant, &none).unwrap();
        assert_eq!(saver.settled(back), None);
        assert_eq!(saver, OverrideSaver::default());
    }

    #[test]
    fn choices_made_during_a_save_end_on_the_last_one() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        sent(&mut saver, EntryMessage, On);
        saver.choose(EntryMessage, Off, &none).unwrap();
        let back = saver.choose(EntryMessage, Participant, &none).unwrap();
        assert_eq!(
            saver.shown(EntryMessage, &server(On)),
            Participant,
            "the first response must not overwrite the newer choice"
        );
        assert_eq!(saver.settled(back), None, "one PATCH in flight");
        assert_eq!(saver.succeeded(), Some(patch(EntryMessage, Participant)));
        assert_eq!(saver.shown(EntryMessage, &server(On)), Participant);
        assert_eq!(saver.succeeded(), None);
        assert_eq!(saver.shown(EntryMessage, &none), Participant);
    }

    #[test]
    fn a_save_landing_mid_wait_leaves_the_newer_choice_to_settle() {
        let mut saver = OverrideSaver::default();
        sent(&mut saver, EntryMessage, On);
        let next = saver
            .choose(ExitSound, Off, &AnnouncementOverrides::default())
            .unwrap();
        assert_eq!(saver.succeeded(), None, "the new choice is still settling");
        assert_eq!(saver.settled(next), Some(patch(ExitSound, Off)));
    }

    #[test]
    fn returning_to_the_in_flight_value_sends_nothing_more() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        sent(&mut saver, ExitSound, Off);
        saver.choose(ExitSound, On, &none).unwrap();
        let back = saver.choose(ExitSound, Off, &none).unwrap();
        assert_eq!(saver.settled(back), None);
        assert_eq!(saver.succeeded(), None);
    }

    #[test]
    fn a_failed_save_restores_the_server_values_and_names_each_field() {
        let mut saver = OverrideSaver::default();
        let none = AnnouncementOverrides::default();
        sent(&mut saver, EntryMessage, On);
        let stale = saver.choose(EntrySound, Off, &none).unwrap();
        assert_eq!(saver.failed(), vec![EntryMessage, EntrySound]);
        let restored = AnnouncementOverrides {
            entry_sound: On,
            ..Default::default()
        };
        assert_eq!(saver.shown(EntryMessage, &restored), Participant);
        assert_eq!(saver.shown(EntrySound, &restored), On);
        assert_eq!(
            override_save_error(&[EntryMessage, EntrySound], &restored),
            "Couldn't save Join message. It's back to \"Their choice\". \
             Couldn't save Join sound. It's back to \"On for everyone\"."
        );
        assert_eq!(
            override_save_error(&[EntryMessage], &server(Off)),
            "Couldn't save Join message. It's back to \"Off for everyone\"."
        );
        let retry = saver.choose(ExitMessage, Off, &none).unwrap();
        assert_eq!(saver.settled(stale), None, "a wait from before the failure");
        assert_eq!(
            saver.settled(retry),
            Some(patch(ExitMessage, Off)),
            "nothing is left in flight after a failure"
        );
    }

    #[test]
    fn a_host_change_is_announced_in_words() {
        let none = AnnouncementOverrides::default();
        assert_eq!(override_change_announcement(&none, &none), None);
        assert_eq!(
            override_change_announcement(&none, &server(Off)).as_deref(),
            Some("The host turned join messages off for everyone.")
        );
        let sounds_on = AnnouncementOverrides {
            entry_sound: On,
            ..Default::default()
        };
        assert_eq!(
            override_change_announcement(&none, &sounds_on).as_deref(),
            Some("The host turned join sounds on for everyone.")
        );
        assert_eq!(
            override_change_announcement(&server(Off), &none).as_deref(),
            Some("Join messages are your choice again.")
        );
    }

    const WIRE_KEYS: [(OverrideField, &str); 4] = [
        (EntryMessage, "entry_message_override"),
        (EntrySound, "entry_sound_override"),
        (ExitMessage, "exit_message_override"),
        (ExitSound, "exit_sound_override"),
    ];

    #[test]
    fn each_response_field_lands_in_its_own_slot() {
        for (field, key) in WIRE_KEYS {
            let status: JoinMeetingResponse = serde_json::from_value(serde_json::json!({
                "user_id": "u", "status": "admitted", "is_host": false, "joined_at": 0,
                key: "off",
            }))
            .unwrap();
            let info: MeetingInfo = serde_json::from_value(serde_json::json!({
                "meeting_id": "m", "state": "active", "host": "h", "has_password": false,
                "waiting_room_enabled": false, "admitted_can_admit": false,
                "end_on_host_leave": false, "participant_count": 1, "waiting_count": 0,
                "started_at": 0,
                key: "on",
            }))
            .unwrap();
            for other in OverrideField::ALL {
                let (want_status, want_info) = if other == field {
                    (Off, On)
                } else {
                    (Participant, Participant)
                };
                assert_eq!(
                    AnnouncementOverrides::from_status(&status).get(other),
                    want_status
                );
                assert_eq!(
                    AnnouncementOverrides::from_meeting_info(&info).get(other),
                    want_info
                );
            }
        }
    }
}
