/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Pure, DOM-free filter & sort logic for the home-page meetings list.
//!
//! The whole feed (≤200 rows) is fetched once into an in-memory `Vec`; this
//! module turns that `Vec` plus a [`FilterState`] / [`SortState`] into the
//! derived list the UI renders. **Everything here is deliberately free of
//! Dioxus, `web_sys`, and `js_sys::Date`** — "now" is injected as a
//! `now_ms` parameter — so the branching logic can be exercised by plain host
//! `#[test]`s under `cargo test -p videocall-ui` (no wasm/DOM harness).
//!
//! ## Filter semantics (see issue #1056)
//!
//! - **Within a group** the selected checkboxes are OR'd (union). Selecting
//!   *both* options in a binary group (e.g. `Owned` + `Not Owned`) imposes no
//!   constraint. Selecting *neither* is also treated as "no constraint" — an
//!   empty group never hides everything, which avoids the surprising
//!   all-empty list when the user unticks the last box.
//! - **Across groups** the constraints are AND'd (intersection): a meeting must
//!   satisfy Ownership AND Status AND the attendance window to be kept.
//! - **Status mapping:** `Active` matches `state == "active"`; `Ended` matches
//!   `state == "ended"`. `"idle"` (created but never activated) is deliberately
//!   neither — it only appears when the Status group is unconstrained.
//! - **Attendance window:** keep meetings whose `user_last_attended_at` falls
//!   within `[now_ms - window, now_ms]`. `Any time` imposes no constraint. A
//!   meeting with `user_last_attended_at == None` (never attended — e.g.
//!   own-but-never-joined) is EXCLUDED by any non-`Any` window.

use serde::{Deserialize, Serialize};
use videocall_meeting_types::responses::MeetingFeedSummary;

/// Milliseconds in a 24-hour day. Window math uses fixed day/month spans
/// rather than calendar arithmetic — the windows are coarse "within the last
/// N" buckets, not precise calendar boundaries.
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
/// Approximate month for the "6 months" bucket: 30 days. Coarse by design.
const MS_PER_MONTH_APPROX: i64 = 30 * MS_PER_DAY;

/// Single-select attendance window. `Any` is the default (no constraint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AttendedWithin {
    #[default]
    Any,
    Last7Days,
    Last30Days,
    Last6Months,
}

impl AttendedWithin {
    /// The window length in milliseconds, or `None` for [`AttendedWithin::Any`]
    /// (no time constraint).
    pub fn window_ms(self) -> Option<i64> {
        match self {
            AttendedWithin::Any => None,
            AttendedWithin::Last7Days => Some(7 * MS_PER_DAY),
            AttendedWithin::Last30Days => Some(30 * MS_PER_DAY),
            AttendedWithin::Last6Months => Some(6 * MS_PER_MONTH_APPROX),
        }
    }

    /// Stable string id for storage / DOM `value` attributes.
    pub fn as_id(self) -> &'static str {
        match self {
            AttendedWithin::Any => "any",
            AttendedWithin::Last7Days => "7d",
            AttendedWithin::Last30Days => "30d",
            AttendedWithin::Last6Months => "6mo",
        }
    }

    /// Parse from the stable id, falling back to the default for anything
    /// unrecognised (forward/backward compatibility with stored values).
    /// Part of the tested string-codec API even though persistence currently
    /// goes through serde; kept symmetric with [`AttendedWithin::as_id`].
    #[allow(dead_code)]
    pub fn from_id(id: &str) -> Self {
        match id {
            "7d" => AttendedWithin::Last7Days,
            "30d" => AttendedWithin::Last30Days,
            "6mo" => AttendedWithin::Last6Months,
            _ => AttendedWithin::Any,
        }
    }

    /// Human label for the radio control.
    pub fn label(self) -> &'static str {
        match self {
            AttendedWithin::Any => "Any time",
            AttendedWithin::Last7Days => "Last 7 days",
            AttendedWithin::Last30Days => "Last 30 days",
            AttendedWithin::Last6Months => "Last 6 months",
        }
    }
}

/// The full set of active filter selections.
///
/// The two binary groups are modelled as a pair of bools rather than a `Vec`
/// of selected variants because each group has exactly two mutually-exclusive
/// options. `(true, true)` and `(false, false)` both mean "no constraint" (see
/// module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterState {
    /// Ownership group — `Owned`.
    pub own_owned: bool,
    /// Ownership group — `Not Owned`.
    pub own_not_owned: bool,
    /// Status group — `Active`.
    pub status_active: bool,
    /// Status group — `Ended`.
    pub status_ended: bool,
    /// Attendance-window single-select.
    pub attended_within: AttendedWithin,
}

impl Default for FilterState {
    /// The default state imposes no constraints: every group is "unconstrained"
    /// and the attendance window is `Any time`. A freshly-defaulted filter
    /// therefore keeps every meeting, so the list matches today's behaviour.
    fn default() -> Self {
        Self {
            own_owned: false,
            own_not_owned: false,
            status_active: false,
            status_ended: false,
            attended_within: AttendedWithin::Any,
        }
    }
}

impl FilterState {
    /// `true` when no group imposes any constraint — i.e. the derived list is
    /// identical to the input. The UI now drives its active-filter affordance
    /// off [`FilterState::active_count`] instead, but this is kept as tested,
    /// symmetric API alongside [`SortState::is_default`].
    #[allow(dead_code)]
    pub fn is_default(&self) -> bool {
        *self == FilterState::default()
    }

    /// Number of *active constraints* for the filter-button count badge.
    ///
    /// A group only counts when it actually narrows the result set, matching
    /// the filter semantics: an Ownership/Status group with both or neither box
    /// ticked imposes no constraint (so contributes 0); a group with exactly
    /// one box ticked contributes 1. The attendance window contributes 1 unless
    /// it is `Any time`. Range: 0..=3.
    pub fn active_count(&self) -> usize {
        let mut n = 0;
        if self.own_owned != self.own_not_owned {
            n += 1;
        }
        if self.status_active != self.status_ended {
            n += 1;
        }
        if self.attended_within != AttendedWithin::Any {
            n += 1;
        }
        n
    }
}

/// The sort key applied after filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SortKey {
    /// By `last_active_at` — the current default ordering.
    #[default]
    LastActive,
    /// By the authenticated user's `user_last_attended_at`.
    LastAttended,
    /// By status rank (active > idle > ended).
    ActiveVsEnded,
    /// Owned meetings first.
    IOwn,
    /// Lexicographic by `meeting_id`.
    MeetingId,
}

impl SortKey {
    /// Stable string id for storage / DOM `value`.
    pub fn as_id(self) -> &'static str {
        match self {
            SortKey::LastActive => "last_active",
            SortKey::LastAttended => "last_attended",
            SortKey::ActiveVsEnded => "active_vs_ended",
            SortKey::IOwn => "i_own",
            SortKey::MeetingId => "meeting_id",
        }
    }

    /// Parse from the stable id, falling back to the default for anything
    /// unrecognised. Part of the tested string-codec API (see
    /// [`AttendedWithin::from_id`]).
    #[allow(dead_code)]
    pub fn from_id(id: &str) -> Self {
        match id {
            "last_attended" => SortKey::LastAttended,
            "active_vs_ended" => SortKey::ActiveVsEnded,
            "i_own" => SortKey::IOwn,
            "meeting_id" => SortKey::MeetingId,
            _ => SortKey::LastActive,
        }
    }

    /// Human label for the dropdown.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::LastActive => "Last active",
            SortKey::LastAttended => "Last attended",
            SortKey::ActiveVsEnded => "Active vs ended",
            SortKey::IOwn => "Owned",
            SortKey::MeetingId => "Meeting ID",
        }
    }

    /// All variants, in the order they should appear in the dropdown.
    pub fn all() -> [SortKey; 5] {
        [
            SortKey::LastActive,
            SortKey::LastAttended,
            SortKey::ActiveVsEnded,
            SortKey::IOwn,
            SortKey::MeetingId,
        ]
    }
}

/// Sort direction. `Desc` is the default (most-recent / owned-first at top).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SortDir {
    Asc,
    #[default]
    Desc,
}

impl SortDir {
    pub fn flipped(self) -> SortDir {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }
}

/// Combined sort selection: which key, which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SortState {
    pub key: SortKey,
    pub dir: SortDir,
}

impl SortState {
    /// `true` when the sort is at its default (`Last active`, descending) — the
    /// ordering the server already applies, i.e. today's behaviour. Symmetric
    /// with [`FilterState::is_default`]; not currently read by the UI (the sort
    /// control is always shown) but kept for parity and covered by tests.
    #[allow(dead_code)]
    pub fn is_default(&self) -> bool {
        *self == SortState::default()
    }
}

/// Status rank for the `Active vs ended` sort. Higher = sorts first under the
/// default descending direction. Active meetings outrank idle, which outrank
/// ended. Any unknown state ranks alongside ended (lowest) so it never
/// surprises the user by floating to the top.
fn status_rank(state: &str) -> i32 {
    match state {
        "active" => 2,
        "idle" => 1,
        _ => 0, // "ended" and any unrecognised state
    }
}

/// Does a single meeting pass the ownership group?
///
/// Returns `true` (no constraint) when both or neither option is selected.
fn passes_ownership(filter: &FilterState, m: &MeetingFeedSummary) -> bool {
    // Both selected or neither selected => no constraint from this group.
    if filter.own_owned == filter.own_not_owned {
        return true;
    }
    if filter.own_owned {
        m.is_owner
    } else {
        // own_not_owned is the only one set.
        !m.is_owner
    }
}

/// Does a single meeting pass the status group?
///
/// Returns `true` (no constraint) when both or neither option is selected.
/// `"idle"` matches neither `Active` nor `Ended`, so it is kept only when the
/// group is unconstrained.
fn passes_status(filter: &FilterState, m: &MeetingFeedSummary) -> bool {
    if filter.status_active == filter.status_ended {
        return true;
    }
    if filter.status_active {
        m.state == "active"
    } else {
        // status_ended is the only one set.
        m.state == "ended"
    }
}

/// Does a single meeting pass the attendance window?
///
/// `Any time` imposes no constraint. For a bounded window the meeting is kept
/// iff `user_last_attended_at` is within `[now_ms - window, now_ms]`. A
/// `None` attendance time (never attended) is always excluded by a bounded
/// window. The lower bound is inclusive (a timestamp exactly `window` ms before
/// `now_ms` is kept).
fn passes_attended_within(filter: &FilterState, m: &MeetingFeedSummary, now_ms: i64) -> bool {
    let Some(window) = filter.attended_within.window_ms() else {
        return true; // Any time
    };
    match m.user_last_attended_at {
        Some(ts) => {
            let lower = now_ms - window;
            ts >= lower && ts <= now_ms
        }
        None => false, // never attended -> excluded by any bounded window
    }
}

/// Apply the [`FilterState`] to the feed, returning the kept rows (cloned).
///
/// Pure: `now_ms` is injected so the attendance window can be tested
/// deterministically.
pub fn filter_meetings(
    meetings: &[MeetingFeedSummary],
    filter: &FilterState,
    now_ms: i64,
) -> Vec<MeetingFeedSummary> {
    meetings
        .iter()
        .filter(|m| {
            passes_ownership(filter, m)
                && passes_status(filter, m)
                && passes_attended_within(filter, m, now_ms)
        })
        .cloned()
        .collect()
}

/// Sort the supplied rows in place per [`SortState`].
///
/// Pure (no `now_ms` needed — sort keys read only stored fields). Every key
/// uses `meeting_id` as the final tiebreak so the order is fully deterministic
/// regardless of input order, which keeps the unit tests stable and avoids
/// flicker when two rows compare equal on the primary key.
pub fn sort_meetings(meetings: &mut [MeetingFeedSummary], sort: &SortState) {
    use std::cmp::Ordering;

    meetings.sort_by(|a, b| {
        // Primary comparison in *ascending* terms; we flip at the end for Desc.
        let primary: Ordering = match sort.key {
            SortKey::LastActive => a.last_active_at.cmp(&b.last_active_at),
            SortKey::LastAttended => {
                // None sorts as "oldest"/lowest so never-attended meetings sink
                // to the bottom under the default descending direction.
                a.user_last_attended_at
                    .unwrap_or(i64::MIN)
                    .cmp(&b.user_last_attended_at.unwrap_or(i64::MIN))
            }
            SortKey::ActiveVsEnded => status_rank(&a.state).cmp(&status_rank(&b.state)),
            SortKey::IOwn => {
                // Owned ranks higher than not-owned so owned floats to the top
                // under descending.
                (a.is_owner as i32).cmp(&(b.is_owner as i32))
            }
            SortKey::MeetingId => a.meeting_id.cmp(&b.meeting_id),
        };

        let primary = match sort.dir {
            SortDir::Asc => primary,
            SortDir::Desc => primary.reverse(),
        };

        // Stable, direction-independent tiebreak: lexicographic meeting_id
        // ascending. Keeping the tiebreak the same regardless of direction
        // means flipping asc/desc only reverses ties at the primary level, not
        // the tiebreak — deterministic either way.
        primary.then_with(|| a.meeting_id.cmp(&b.meeting_id))
    });
}

/// Convenience: filter then sort, returning the derived list. This is the
/// single entry point the `use_memo` in the component calls.
pub fn filter_and_sort_meetings(
    meetings: &[MeetingFeedSummary],
    filter: &FilterState,
    sort: &SortState,
    now_ms: i64,
) -> Vec<MeetingFeedSummary> {
    let mut out = filter_meetings(meetings, filter, now_ms);
    sort_meetings(&mut out, sort);
    out
}

#[cfg(test)]
mod tests {
    //! Host-target unit tests for the PURE filter/sort logic. No wasm/DOM
    //! needed — `now_ms` is injected — so these run under
    //! `cargo test -p videocall-ui`.
    use super::*;

    /// Base sample row. Tests override only the fields they care about. Mirrors
    /// the `sample_meeting` helper in `meetings_list.rs` but lives here so the
    /// pure tests don't depend on the wasm test module.
    fn meeting(id: &str) -> MeetingFeedSummary {
        MeetingFeedSummary {
            meeting_id: id.to_string(),
            state: "active".to_string(),
            last_active_at: 1_000,
            created_at: 500,
            started_at: Some(900),
            ended_at: None,
            host: Some("alice@example.com".to_string()),
            host_display_name: Some("Alice".to_string()),
            host_user_id: Some("alice@example.com".to_string()),
            is_owner: false,
            participant_count: 0,
            waiting_count: 0,
            has_password: false,
            allow_guests: false,
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
            user_last_attended_at: None,
        }
    }

    fn ids(v: &[MeetingFeedSummary]) -> Vec<&str> {
        v.iter().map(|m| m.meeting_id.as_str()).collect()
    }

    // ---- Ownership group ----------------------------------------------------

    #[test]
    fn ownership_neither_selected_is_no_constraint() {
        let mut owned = meeting("a");
        owned.is_owner = true;
        let not_owned = meeting("b");
        let set = vec![owned, not_owned];
        let f = FilterState::default(); // neither ownership box ticked
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out).len(), 2, "empty group must not constrain");
    }

    #[test]
    fn ownership_both_selected_is_no_constraint() {
        let mut owned = meeting("a");
        owned.is_owner = true;
        let not_owned = meeting("b");
        let set = vec![owned, not_owned];
        let f = FilterState {
            own_owned: true,
            own_not_owned: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(out.len(), 2, "both ticked == union == no constraint");
    }

    #[test]
    fn ownership_owned_only_keeps_owned() {
        let mut owned = meeting("a");
        owned.is_owner = true;
        let not_owned = meeting("b");
        let set = vec![owned, not_owned];
        let f = FilterState {
            own_owned: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out), vec!["a"]);
    }

    #[test]
    fn ownership_not_owned_only_keeps_not_owned() {
        let mut owned = meeting("a");
        owned.is_owner = true;
        let not_owned = meeting("b");
        let set = vec![owned, not_owned];
        let f = FilterState {
            own_not_owned: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out), vec!["b"]);
    }

    // ---- Status group -------------------------------------------------------

    #[test]
    fn status_active_only_keeps_active_excludes_idle_and_ended() {
        let active = meeting("a"); // default state == active
        let mut idle = meeting("b");
        idle.state = "idle".to_string();
        let mut ended = meeting("c");
        ended.state = "ended".to_string();
        let set = vec![active, idle, ended];
        let f = FilterState {
            status_active: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out), vec!["a"]);
    }

    #[test]
    fn status_ended_only_keeps_ended_excludes_idle_and_active() {
        let active = meeting("a");
        let mut idle = meeting("b");
        idle.state = "idle".to_string();
        let mut ended = meeting("c");
        ended.state = "ended".to_string();
        let set = vec![active, idle, ended];
        let f = FilterState {
            status_ended: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out), vec!["c"]);
    }

    #[test]
    fn status_idle_only_shows_when_group_unconstrained() {
        let mut idle = meeting("b");
        idle.state = "idle".to_string();
        let set = vec![idle];
        // Unconstrained (neither box) -> idle kept.
        let out = filter_meetings(&set, &FilterState::default(), 0);
        assert_eq!(ids(&out), vec!["b"]);
        // Both boxes (still no constraint) -> idle kept.
        let f = FilterState {
            status_active: true,
            status_ended: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        assert_eq!(ids(&out), vec!["b"]);
    }

    // ---- Across-group intersection -----------------------------------------

    #[test]
    fn cross_group_intersection_owner_and_active() {
        let mut owned_active = meeting("a");
        owned_active.is_owner = true;
        let mut owned_ended = meeting("b");
        owned_ended.is_owner = true;
        owned_ended.state = "ended".to_string();
        let active_not_owned = meeting("c"); // active, not owned
        let set = vec![owned_active, owned_ended, active_not_owned];
        let f = FilterState {
            own_owned: true,
            status_active: true,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 0);
        // Only the owned AND active row survives.
        assert_eq!(ids(&out), vec!["a"]);
    }

    // ---- Attendance window --------------------------------------------------

    #[test]
    fn attended_none_excluded_by_7d_window() {
        let never = meeting("a"); // user_last_attended_at == None
        let set = vec![never];
        let f = FilterState {
            attended_within: AttendedWithin::Last7Days,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, 1_000_000_000);
        assert!(out.is_empty(), "never-attended must be excluded by 7d");
    }

    #[test]
    fn attended_any_time_keeps_none() {
        let never = meeting("a");
        let set = vec![never];
        let out = filter_meetings(&set, &FilterState::default(), 1_000_000_000);
        assert_eq!(out.len(), 1, "Any time imposes no attendance constraint");
    }

    #[test]
    fn attended_window_boundary_just_inside_and_just_outside() {
        let now = 100 * MS_PER_DAY; // arbitrary "now"
        let window = 7 * MS_PER_DAY;
        let mut inside = meeting("inside");
        inside.user_last_attended_at = Some(now - window); // exactly on lower bound (inclusive)
        let mut just_inside = meeting("just_inside");
        just_inside.user_last_attended_at = Some(now - window + 1);
        let mut outside = meeting("outside");
        outside.user_last_attended_at = Some(now - window - 1); // 1ms too old
        let set = vec![inside, just_inside, outside];
        let f = FilterState {
            attended_within: AttendedWithin::Last7Days,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, now);
        let mut got = ids(&out);
        got.sort();
        assert_eq!(got, vec!["inside", "just_inside"]);
    }

    #[test]
    fn attended_future_timestamp_excluded() {
        // A timestamp after "now" (clock skew) falls outside [now-window, now].
        let now = 100 * MS_PER_DAY;
        let mut future = meeting("future");
        future.user_last_attended_at = Some(now + 1);
        let set = vec![future];
        let f = FilterState {
            attended_within: AttendedWithin::Last30Days,
            ..FilterState::default()
        };
        let out = filter_meetings(&set, &f, now);
        assert!(out.is_empty(), "future attendance must be excluded");
    }

    // ---- Sort: each key, both directions -----------------------------------

    fn sample_for_sort() -> Vec<MeetingFeedSummary> {
        let mut a = meeting("a");
        a.last_active_at = 300;
        a.is_owner = true;
        a.state = "active".to_string();
        a.user_last_attended_at = Some(50);

        let mut b = meeting("b");
        b.last_active_at = 100;
        b.is_owner = false;
        b.state = "ended".to_string();
        b.user_last_attended_at = Some(200);

        let mut c = meeting("c");
        c.last_active_at = 200;
        c.is_owner = false;
        c.state = "idle".to_string();
        c.user_last_attended_at = None;

        vec![a, b, c]
    }

    #[test]
    fn sort_last_active_desc_and_asc() {
        let mut v = sample_for_sort();
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::LastActive,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(ids(&v), vec!["a", "c", "b"]); // 300,200,100
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::LastActive,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(ids(&v), vec!["b", "c", "a"]);
    }

    #[test]
    fn sort_last_attended_desc_pushes_none_to_bottom() {
        let mut v = sample_for_sort();
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::LastAttended,
                dir: SortDir::Desc,
            },
        );
        // b=200, a=50, c=None(MIN) -> b, a, c
        assert_eq!(ids(&v), vec!["b", "a", "c"]);
    }

    #[test]
    fn sort_active_vs_ended_desc() {
        let mut v = sample_for_sort();
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::ActiveVsEnded,
                dir: SortDir::Desc,
            },
        );
        // active(a)=2 > idle(c)=1 > ended(b)=0
        assert_eq!(ids(&v), vec!["a", "c", "b"]);
    }

    #[test]
    fn sort_i_own_desc_owned_first() {
        let mut v = sample_for_sort();
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::IOwn,
                dir: SortDir::Desc,
            },
        );
        // a owned -> first; b,c not owned -> tiebreak by meeting_id asc
        assert_eq!(ids(&v), vec!["a", "b", "c"]);
    }

    #[test]
    fn sort_meeting_id_asc_and_desc() {
        let mut v = sample_for_sort();
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::MeetingId,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(ids(&v), vec!["a", "b", "c"]);
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::MeetingId,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(ids(&v), vec!["c", "b", "a"]);
    }

    #[test]
    fn sort_tiebreak_is_meeting_id_when_primary_equal() {
        // Three rows with identical last_active_at: order must be deterministic
        // by meeting_id regardless of input order.
        let mut x = meeting("zeta");
        x.last_active_at = 500;
        let mut y = meeting("alpha");
        y.last_active_at = 500;
        let mut z = meeting("mid");
        z.last_active_at = 500;
        let mut v = vec![x, y, z];
        sort_meetings(
            &mut v,
            &SortState {
                key: SortKey::LastActive,
                dir: SortDir::Desc,
            },
        );
        // All equal on primary -> tiebreak meeting_id ascending.
        assert_eq!(ids(&v), vec!["alpha", "mid", "zeta"]);
    }

    // ---- Default filter+sort reproduces today's behaviour ------------------

    #[test]
    fn default_filter_and_sort_is_last_active_desc_over_sample() {
        let mut m1 = meeting("m1");
        m1.last_active_at = 100;
        let mut m2 = meeting("m2");
        m2.last_active_at = 300;
        let mut m3 = meeting("m3");
        m3.last_active_at = 200;
        let set = vec![m1, m2, m3];
        let out = filter_and_sort_meetings(
            &set,
            &FilterState::default(),
            &SortState::default(),
            999_999,
        );
        // No rows dropped; ordered last_active_at DESC -> m2(300), m3(200), m1(100).
        assert_eq!(ids(&out), vec!["m2", "m3", "m1"]);
    }

    #[test]
    fn is_default_flags() {
        assert!(FilterState::default().is_default());
        assert!(SortState::default().is_default());
        let f = FilterState {
            own_owned: true,
            ..FilterState::default()
        };
        assert!(!f.is_default());
    }

    #[test]
    fn active_count_matches_constraint_semantics() {
        // Default -> nothing narrows -> 0.
        assert_eq!(FilterState::default().active_count(), 0);
        // Both boxes in a group == no constraint -> still 0.
        let both_own = FilterState {
            own_owned: true,
            own_not_owned: true,
            ..FilterState::default()
        };
        assert_eq!(both_own.active_count(), 0);
        // Exactly one box in each binary group + a bounded window -> 3.
        let all_three = FilterState {
            own_owned: true,
            status_active: true,
            attended_within: AttendedWithin::Last7Days,
            ..FilterState::default()
        };
        assert_eq!(all_three.active_count(), 3);
        // Window alone -> 1.
        let window_only = FilterState {
            attended_within: AttendedWithin::Last30Days,
            ..FilterState::default()
        };
        assert_eq!(window_only.active_count(), 1);
    }

    #[test]
    fn id_roundtrip_for_enums() {
        for k in SortKey::all() {
            assert_eq!(SortKey::from_id(k.as_id()), k);
        }
        for w in [
            AttendedWithin::Any,
            AttendedWithin::Last7Days,
            AttendedWithin::Last30Days,
            AttendedWithin::Last6Months,
        ] {
            assert_eq!(AttendedWithin::from_id(w.as_id()), w);
        }
        // Unrecognised -> defaults.
        assert_eq!(SortKey::from_id("bogus"), SortKey::default());
        assert_eq!(AttendedWithin::from_id("bogus"), AttendedWithin::default());
    }
}
