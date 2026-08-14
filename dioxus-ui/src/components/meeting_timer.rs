/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Host-set meeting countdown, visible to every participant (issue 2136).
//!
//! Everything except the components themselves is pure and driven by plain
//! `#[test]` — the only test gate that reliably executes for this crate. The
//! wire-level policy (last-writer-wins, saturating arithmetic, the monotonic
//! countdown sample, the send cadence) lives one layer down in
//! `videocall_client::client::meeting_timer` (re-exported at the crate root)
//! and is tested there; this module
//! owns only the presentation decisions on top of it.
//!
//! # Re-render blast radius
//!
//! This is the load-bearing structural property of the module, so it is stated
//! up front rather than left to be inferred.
//!
//! A countdown ticks once per second for the whole life of a timer. If that tick
//! dirtied a signal the tile grid subscribes to, every `PeerTile` in the meeting
//! would re-render 60 times a minute — the #2103 / #2135 blast-radius hazard,
//! twice over. Two mechanisms prevent it, and they are independent:
//!
//! 1. **The per-second tick writes a signal that does not exist outside
//!    [`MeetingTimerChip`].** `remaining_ms` is created by `use_signal` INSIDE
//!    that component, so no other scope can subscribe to it — this is true by
//!    construction, not by convention, and cannot be broken by an edit elsewhere.
//! 2. **A heartbeat never dirties the shared state signal.** The host re-sends
//!    the current state every ~5s; [`would_apply_change`] returns `false` for a
//!    re-send of the applied state, so `MeetingTimerCtx` is not written and even
//!    the chip does not re-render. Only a genuine transition (start / extend /
//!    cancel) writes it.
//!
//! `MeetingTimerCtx` is deliberately read ONLY inside this module, by exactly
//! FOUR components — [`MeetingTimerChip`], [`MeetingTimerLiveRegion`],
//! [`MeetingTimerDockControl`] and [`MeetingTimerPopover`] — and never by
//! `AttendantsComponent` itself, for the same reason `RaisedHandsBanner` reads
//! `RaisedHandsCtx` rather than taking it as a prop: a context read in the
//! attendants body would subscribe that ~9,000-line RSX, and every keyed
//! `PeerTile` under it, to every timer change. Each of the four renders a
//! handful of nodes, so a transition re-renders only those.
//!
//! `AttendantsComponent` owns the signal and PROVIDES it, and touches its value
//! only through `peek()` (in the inbound callback, the transition helper, and
//! the reconnect drop) — `peek` does not subscribe. Verified by grep: there is
//! no `use_context::<MeetingTimerCtx>()` anywhere outside this file.

use crate::components::video_control_buttons::MeetingTimerButton;
use crate::context::DockPositionCtx;
use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use videocall_client::{clamp_duration_ms, CountdownSample, MeetingTimerState};

/// Tick period for the visible countdown. One second: the countdown is rendered
/// at second granularity, so a faster tick would repaint identical text.
pub const MEETING_TIMER_TICK_MS: u32 = 1_000;

/// Nominal CRITICAL threshold. A CAP, not the value used directly: [`urgency`]
/// takes `min(this, duration/4)`, so on a short timer the proportional term wins
/// (the 60s preset goes critical at 15s, not 30s).
pub const MEETING_TIMER_CRITICAL_MS: u64 = 30_000;

/// Nominal WARNING threshold. A CAP in the middle of the range, NOT an absolute
/// floor, and NOT OR-ed with anything — both of which this doc claimed until
/// issue 2136's threshold fix, and neither of which was ever true of the shipped
/// `max(min(this, duration/2), duration/4)`.
///
/// It is the operative value only for durations between 240s and 480s. Below
/// that a proportional term tightens it (or a 60s timer would start amber and
/// never render Normal); above 480s a proportional term LOOSENS it, so this
/// value never applies to a long session at all. See [`urgency`] for the full
/// piecewise table.
pub const MEETING_TIMER_WARNING_MS: u64 = 120_000;

/// Duration presets offered to the host, in milliseconds.
pub const MEETING_TIMER_PRESETS_MS: &[u64] = &[60_000, 300_000, 600_000, 900_000];

/// The increment applied by the "+1 min" control.
pub const MEETING_TIMER_EXTEND_STEP_MS: u64 = 60_000;

/// Points at which the screen-reader live region speaks, in ms remaining.
///
/// A countdown that announced every second would make the meeting unusable with
/// a screen reader: it would speak over every other participant and over every
/// other live region on the page. These are the only instants that carry
/// information a listener can act on.
pub const MEETING_TIMER_ANNOUNCE_MILESTONES_MS: &[u64] = &[300_000, 60_000, 30_000, 10_000, 0];

/// Visual urgency of the countdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerUrgency {
    Normal,
    Warning,
    Critical,
    Expired,
}

impl TimerUrgency {
    /// CSS modifier suffix for `.meeting-timer-chip--{}`.
    pub fn modifier(self) -> &'static str {
        match self {
            TimerUrgency::Normal => "normal",
            TimerUrgency::Warning => "warning",
            TimerUrgency::Critical => "critical",
            TimerUrgency::Expired => "expired",
        }
    }
}

/// Urgency for `remaining_ms` against a timer of `duration_ms`.
///
/// Both thresholds are the flat value CAPPED BY A PROPORTION of the timer's own
/// length, which is what keeps all three states reachable across the whole range
/// of durations this feature ships.
///
/// The flat values alone do not work at the short end. With a bare
/// `remaining <= MEETING_TIMER_WARNING_MS` floor, the 60s preset in
/// [`MEETING_TIMER_PRESETS_MS`] satisfies it at t=0 (`60_000 <= 120_000`), so it
/// would start amber and go red for its last 30 seconds — one of the three
/// visual states unreachable on the shortest preset we ship. Capping warning at
/// `duration/2` and critical at `duration/4` gives that preset a real
/// Normal 60→30, Warning 30→15, Critical 15→0.
///
/// CRITICAL is the flat 30s, tightened proportionally only where 30s would be too
/// much of the timer: `min(30s, duration/4)`.
///
/// WARNING is piecewise, and is NOT simply "whichever is tighter" — that was an
/// earlier description of this code and it was wrong above 480s.
/// `max(min(120s, duration/2), duration/4)` resolves to:
///
/// | duration        | Warning at   | why                                     |
/// |---|---|---|
/// | `<= 240s`       | `duration/2` | proportional, so short timers keep a Normal phase |
/// | `240s..=480s`   | `120s` flat  | the flat cap is the tightest term here  |
/// | `> 480s`        | `duration/4` | proportional again, and LOOSER than the flat cap |
///
/// So the proportional terms bracket the flat one at BOTH ends, for opposite
/// reasons: below 240s they tighten it (or a 60s timer starts amber and never
/// renders Normal), and above 480s they loosen it (a 2-minute warning on a
/// 45-minute slot is no notice at all — that duration warns at 675s, i.e. 11m15s
/// of amber, which is deliberate).
///
/// WHAT THE `.max(duration/4)` IS NOT FOR: it does not keep warning above
/// critical. That holds without it for every duration — below 240s because
/// `duration/2 >= duration/4 >= crit`, and above because `120s > 30s`. Its only
/// effect is the third row above.
///
/// Boundaries for the shipped presets, pinned by
/// `tests::preset_urgency_boundaries_are_pinned`:
///
/// | duration | Warning at | Critical at |
/// |---|---|---|
/// |  60s |  30s | 15s |
/// | 300s | 120s | 30s |
/// | 600s | 150s | 30s |
/// | 900s | 225s | 30s |
///
/// Amber tops out at 3m45s across the presets. It is unbounded in principle —
/// [`MEETING_TIMER_MAX_DURATION_MS`] is 24h and "+1 min" has no ceiling below it
/// — so a 24h timer would sit amber for 6 hours. Nothing renders worse for it
/// (the value and the accessible name still count down exactly), which is why
/// this is documented rather than clamped.
pub fn urgency(remaining_ms: u64, duration_ms: u64) -> TimerUrgency {
    if remaining_ms == 0 {
        return TimerUrgency::Expired;
    }
    // Division, never multiplication: `remaining * 4 <= duration` overflows for a
    // large remaining, `remaining <= duration / 4` cannot.
    let crit_at = MEETING_TIMER_CRITICAL_MS.min(duration_ms / 4);
    let warn_at = MEETING_TIMER_WARNING_MS
        .min(duration_ms / 2)
        .max(duration_ms / 4);
    if remaining_ms <= crit_at {
        return TimerUrgency::Critical;
    }
    if remaining_ms <= warn_at {
        return TimerUrgency::Warning;
    }
    TimerUrgency::Normal
}

/// Format remaining milliseconds as `M:SS`, or `H:MM:SS` past an hour.
///
/// Rounds UP to the next whole second while any time remains, so the countdown
/// shows `1:00` for the whole of the final minute rather than flicking to `0:59`
/// immediately — and, more importantly, never shows `0:00` while the timer is
/// still running. `0:00` means expired, and it must mean only that, because that
/// is the instant the room hears a sound.
pub fn format_remaining(remaining_ms: u64) -> String {
    let total_secs = remaining_ms.div_ceil(1_000);
    let hours = total_secs / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Human-readable duration for announcements and control labels ("5 minutes").
///
/// Grows an HOURS field past an hour, which is issue 2172's doing. Before the
/// typed duration the longest thing a host could set in one action was the
/// 15-minute preset, and the only route past an hour was clicking "+1 min" a few
/// dozen times — so an hour-plus wording was barely reachable. Typing a duration
/// puts the whole range up to the 24h cap one keystroke away, and "Start a 1440
/// minutes timer" is not an accessible name anyone can parse.
///
/// Zero renders as "0 seconds" rather than the empty string: it is the wording
/// [`compose_transition_announcement`] and [`compose_chip_label`] fall back to,
/// and an empty duration in the middle of a sentence reads as a bug.
pub fn format_duration_words(duration_ms: u64) -> String {
    let total_secs = duration_ms / 1_000;
    let hours = total_secs / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours} hour{}", plural(hours)));
    }
    if minutes > 0 {
        parts.push(format!("{minutes} minute{}", plural(minutes)));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds} second{}", plural(seconds)));
    }
    parts.join(" ")
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Whether an inbound state should be WRITTEN to the shared signal.
///
/// This is the guard that keeps the ~5s heartbeat from dirtying every subscriber
/// of `MeetingTimerCtx`. It is the conjunction of two independent conditions:
///
///  * [`should_apply`](videocall_client::should_apply) —
///    the bounded-window last-writer-wins rule, which suppresses a packet
///    REORDERED on the WebTransport datagram path; and
///  * a plain inequality — which suppresses a packet that is perfectly ordered
///    and perfectly valid but carries the state we already hold, i.e. every
///    heartbeat and every transition repeat after the first.
///
/// Both are needed. Dropping the first resurrects cancelled timers; dropping the
/// second turns a 5-second heartbeat into a 5-second re-render of everything
/// subscribed to the timer.
pub fn would_apply_change(applied: Option<MeetingTimerState>, incoming: MeetingTimerState) -> bool {
    videocall_client::should_apply(applied, incoming) && applied != Some(incoming)
}

/// Identity of a timer state, used to key the expiry cue: `(ends_at_ms,
/// updated_at_ms)`.
pub type TimerIdentity = (u64, u64);

/// Fires the expiry cue at most once per timer, and ONLY for a zero the client
/// actually watched arrive.
///
/// The distinction is the whole point. A zero that was already true when the
/// state first reached us is HISTORY, not an event — and it is routinely
/// reachable, because the host keeps heartbeating an expired timer for a grace
/// period. Without this, every late joiner and every page reload heard the alarm
/// about a second after entering, for a timer that may have run out minutes
/// earlier, in front of a room where nothing had just happened.
///
/// Keyed on the state IDENTITY rather than a bool, so a timer the host EXTENDS
/// past now gets a fresh countdown and can ring properly when it really does
/// reach zero — no flag anywhere has to remember to reset.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExpiryLatch {
    fired_for: Option<TimerIdentity>,
}

impl ExpiryLatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly SAMPLED state. Seeds the latch when the countdown is
    /// already at zero at the moment of sampling, so the cue can never fire for
    /// a zero this client did not witness.
    pub fn observe_sample(&mut self, identity: TimerIdentity, remaining_ms: u64) {
        if remaining_ms == 0 {
            self.fired_for = Some(identity);
        }
    }

    /// Called on each countdown tick. Returns `true` exactly once, for the tick
    /// on which a watched timer crosses zero.
    pub fn should_ring(&mut self, identity: TimerIdentity, remaining_ms: u64) -> bool {
        if remaining_ms != 0 || self.fired_for == Some(identity) {
            return false;
        }
        self.fired_for = Some(identity);
        true
    }
}

/// Whether a (re)connect should DROP the locally-held meeting-timer state.
///
/// `driving` is what THIS client's own scheduler is currently announcing (i.e.
/// `MeetingTimerScheduler::current()`); `applied` is what we are rendering.
///
/// This closes the one hole the heartbeat cannot repair. Rule 1 re-announces a
/// RUNNING timer every ~5s, so a viewer that missed a start or an extend
/// converges on its own. A CANCEL has no heartbeat behind it — once `running` is
/// false the host goes quiet — so its only delivery is the 3-packet transition
/// burst, and a viewer whose connection was down across that burst keeps a timer
/// the host already stopped, counts it to zero, and PLAYS THE EXPIRY SOUND at
/// nobody. Dropping instead costs at most one heartbeat of a running timer not
/// being shown: silent, self-healing, and strictly the better failure.
///
/// The test is "am I DRIVING this timer", NOT "am I the host". The host never
/// receives its own packet back (the relay self-skips the sender), so its chip is
/// a local echo with nothing to re-establish it — dropping there would blank the
/// host's own timer until it touched the controls again. The host FLAG is
/// separately the wrong input: it can be stale at exactly this moment, which is
/// why a roster reseed runs on the same event.
pub fn should_drop_timer_on_connect(
    driving: Option<MeetingTimerState>,
    applied: Option<MeetingTimerState>,
) -> bool {
    applied.is_some() && !driving.is_some_and(|s| s.running)
}

/// Unit for the host's typed duration (issue 2172).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomDurationUnit {
    Seconds,
    Minutes,
}

impl CustomDurationUnit {
    /// Milliseconds in one of this unit.
    pub fn ms(self) -> u64 {
        match self {
            CustomDurationUnit::Seconds => 1_000,
            CustomDurationUnit::Minutes => 60_000,
        }
    }

    /// The `<option value>` this unit is carried as, and the string the change
    /// handler reads back off the `<select>`.
    pub fn as_value(self) -> &'static str {
        match self {
            CustomDurationUnit::Seconds => "seconds",
            CustomDurationUnit::Minutes => "minutes",
        }
    }

    /// The unit for an `<option value>`, or `None` for anything that is not one
    /// of the two options rendered.
    ///
    /// `None` leaves the current unit ALONE rather than resolving to a default.
    /// The only writer of this string is the `<option>` list in
    /// [`MeetingTimerPopover`], so an unrecognised value means the DOM was
    /// tampered with, and quietly picking some unit would start a timer of a
    /// length the host never chose.
    pub fn from_value(raw: &str) -> Option<Self> {
        match raw {
            "seconds" => Some(CustomDurationUnit::Seconds),
            "minutes" => Some(CustomDurationUnit::Minutes),
            _ => None,
        }
    }
}

/// The unit the custom entry OPENS on.
///
/// Minutes, matching the unit every preset button directly above it is labelled
/// in ("5 min"), so a number the host types reads in the same unit as the row
/// they just looked at. It also errs in the safer direction: a mis-set value is
/// then too LONG, rather than a 5-second timer that buzzes at the whole room a
/// moment after the host meant to give a speaker five minutes.
pub const MEETING_TIMER_CUSTOM_DEFAULT_UNIT: CustomDurationUnit = CustomDurationUnit::Minutes;

/// Parse the host's typed duration into milliseconds (issue 2172).
///
/// THE single source of truth for the custom row: the confirm button's disabled
/// state, its accessible name, the hint beneath it and the value actually handed
/// to `on_start` all read this one result, so what the button says, whether it
/// can be clicked, and what the room gets cannot disagree.
///
/// `None` means "not a duration", and DISABLES the confirm button. The accepted
/// shape is exactly "optional surrounding space, then one or more ASCII digits,
/// not all zero" — so empty, whitespace, `0`, `-5`, `+5`, `1.5`, `1e3`, `5m` and
/// non-ASCII digits are all refused. Rejecting rather than coercing matters
/// because the alternative is a live-looking button that starts something the
/// host did not type.
///
/// The digit check is deliberately made BEFORE the parse rather than as a
/// fallback after it, because `u64::from_str` accepts a leading `+`: checking
/// afterwards would take `+5` (parses) but refuse a `+` followed by an
/// over-length run of digits (does not parse, is not all digits), which is an
/// asymmetry nobody could predict from the outside.
///
/// An amount ABOVE the ceiling CLAMPS to [`MEETING_TIMER_MAX_DURATION_MS`]
/// instead of rejecting, for two reasons:
///
///  * the relay DROPS an over-cap START at ingress rather than clamping it
///    (`packet_handler.rs` checks `duration_ms <= MEETING_TIMER_MAX_DURATION_MS`
///    and returns `Dropped`), so an unclamped value would leave the host looking
///    at a local echo of a timer no other participant can see; and
///  * disabling on an over-cap value would be visually indistinguishable from
///    disabling on junk, when a host typing "2000" minutes plainly wants the
///    longest timer available.
///
/// The clamped value is what the button's accessible name and the visible hint
/// spell out, so the cap is stated BEFORE it is applied rather than discovered
/// after — a host who types 2000 minutes reads "Timer will run for 24 hours."
/// while the entry still has focus.
pub fn custom_duration_ms(value: &str, unit: CustomDurationUnit) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Guaranteed by the check above to be a non-empty run of ASCII digits, so the
    // ONLY way this parse can fail is overflow — which is still a well-formed
    // request for more time than exists, and takes the same clamp as "2000
    // minutes" rather than blanking the button.
    let amount = trimmed.parse::<u64>().unwrap_or(u64::MAX);
    if amount == 0 {
        return None;
    }
    // `saturating_mul` before the clamp, and the danger is not that a wrapped
    // product is obviously wrong — it is that it lands back INSIDE the cap and
    // reads as a plausible short timer. The smallest minute count that overflows
    // is 307_445_734_561_826; `scaling_to_milliseconds_saturates_rather_than_wrapping`
    // pins the seconds witness 18_446_744_073_709_552, whose wrapped product is
    // exactly 384 ms — a third-of-a-second timer from a host asking for longer
    // than the universe.
    Some(clamp_duration_ms(amount.saturating_mul(unit.ms())))
}

/// Build the state for a host STARTING a timer of `duration_ms` at `now_ms`.
///
/// `duration_ms` is clamped so the relay cannot reject the packet at ingress: an
/// over-cap duration DROPS rather than clamps server-side, and a dropped START
/// would leave the host looking at a local echo of a timer no one else can see.
pub fn start_state(duration_ms: u64, now_ms: u64) -> MeetingTimerState {
    let duration_ms = clamp_duration_ms(duration_ms);
    MeetingTimerState {
        running: true,
        ends_at_ms: now_ms.saturating_add(duration_ms),
        duration_ms,
        updated_at_ms: now_ms,
    }
}

/// Build the state for a host EXTENDING the running timer by `extra_ms`.
///
/// Two details the proto pins and this must honour:
///
///  * `duration_ms` is the TOTAL span including the extension (5 min extended to
///    8 is `8`, not `3`), so it is recomputed from the ORIGINAL start rather than
///    incremented — that keeps `started_at_ms = ends_at_ms - duration_ms` stable
///    and the rendered progress proportion continuous across the extension.
///  * an already-EXPIRED timer extends from NOW, not from its stale end instant.
///    A host clicking "+1 min" after the buzzer means "one more minute", not "one
///    minute measured from a moment that has already passed" — which for a timer
///    that expired two minutes ago would produce a still-expired result and an
///    apparently dead button.
pub fn extend_state(current: MeetingTimerState, extra_ms: u64, now_ms: u64) -> MeetingTimerState {
    let started_at_ms = current.started_at_ms();
    let base = current.ends_at_ms.max(now_ms);
    let ends_at_ms = base.saturating_add(extra_ms);
    // Clamp the TOTAL, then re-derive the end instant from it, so the two can
    // never disagree — `ends_at_ms < duration_ms` is rejected by the relay at
    // ingress and underflows `started_at_ms` on any peer running an older one.
    let duration_ms = clamp_duration_ms(ends_at_ms.saturating_sub(started_at_ms));
    MeetingTimerState {
        running: true,
        ends_at_ms: started_at_ms.saturating_add(duration_ms),
        duration_ms,
        updated_at_ms: now_ms,
    }
}

/// The LOWEST milestone crossed between two consecutive remaining-time readings.
///
/// "Crossed" means `prev` was strictly above it and `now` is at or below it.
///
/// LOWEST, not highest, and that is the whole subtlety. A tick can span several
/// milestones at once — a backgrounded tab is throttled to roughly one timer
/// callback a minute, so a 60s timer can go straight from 60_000 to 0 in a single
/// tick. Reporting the highest crossed would announce "30 seconds remaining" and
/// then, with `prev == remaining == 0` from then on, never cross anything again:
/// **"Time is up." would never be spoken at all**, on precisely the timers whose
/// expiry the listener most needs to hear about.
///
/// Only one utterance is emitted per tick by design — a live region that is
/// rewritten several times in a frame announces only its final value anyway, so
/// draining every crossed milestone would buy nothing over taking the last one.
pub fn milestone_crossed(
    prev_remaining_ms: u64,
    remaining_ms: u64,
    duration_ms: u64,
) -> Option<u64> {
    MEETING_TIMER_ANNOUNCE_MILESTONES_MS
        .iter()
        .copied()
        .filter(|&m| announces_milestone(m, duration_ms))
        .rfind(|&m| prev_remaining_ms > m && remaining_ms <= m)
}

/// Whether `milestone_ms` is worth speaking for a timer of `duration_ms`.
///
/// Drops the 10-second call on a timer of a minute or less. Two reasons, and the
/// second is the stronger one:
///
///  * DENSITY. On the 60s preset the region would otherwise speak at 30s, 10s and
///    zero — three utterances inside the final thirty seconds, over a presenter
///    who is by definition still talking.
///  * ALIGNMENT. That preset's visual states change at 30s (Warning) and 15s
///    (Critical). A 10s announcement matches NEITHER: it is speech with no visual
///    counterpart, arriving five seconds after a visual change that had no
///    speech. Dropping it leaves 30s and zero, which line up exactly with Warning
///    and Expired.
///
/// Deliberately NOT generalised into "announce on urgency transitions instead of
/// fixed milestones", which would align speech and visuals at every duration.
/// That is the better design and it is a larger change than this round should
/// carry; the fixed ladder is still correct for the longer presets, where a 10s
/// call is a useful last warning rather than noise.
fn announces_milestone(milestone_ms: u64, duration_ms: u64) -> bool {
    !(milestone_ms == 10_000 && duration_ms <= 60_000)
}

/// Live-region text for a milestone.
pub fn compose_milestone_announcement(milestone_ms: u64) -> String {
    if milestone_ms == 0 {
        "Time is up.".to_string()
    } else {
        format!("{} remaining.", format_duration_words(milestone_ms))
    }
}

/// Live-region text for a timer state TRANSITION (start / extend / cancel).
pub fn compose_transition_announcement(state: MeetingTimerState, remaining_ms: u64) -> String {
    if !state.running {
        return "Meeting timer cancelled.".to_string();
    }
    // ALREADY EXPIRED on arrival. Reachable, and not rare: the host keeps
    // heartbeating an expired timer for a grace window, so anyone who joins or
    // reloads inside it observes `running == true` with zero remaining.
    //
    // Without this branch that case took the "set" branch below and announced
    // "Meeting timer set. 0 seconds remaining." — present tense, as though a
    // timer had just begun, and never saying it had expired.
    //
    // The milestone path CANNOT cover this. A milestone needs a CROSSING, and the
    // effect that produces this announcement has just recorded `last_remaining`
    // as 0, so the first poll evaluates `milestone_crossed(0, 0, ..)` — `0 > 0` is
    // false — and returns `None` forever after. That is pinned by
    // `no_milestone_is_reported_when_none_is_crossed`.
    //
    // The expiry TONE used to carry this meaning by accident. Suppressing the
    // false beep for late joiners (`ExpiryLatch`) was correct, but it removed the
    // thing that had been masking this: the announcement was always wrong, and
    // silencing the sound is what made the wrongness audible only to a screen
    // reader.
    if remaining_ms == 0 {
        return "Meeting timer: time is up.".to_string();
    }
    format!(
        "Meeting timer set. {} remaining.",
        format_duration_words(remaining_ms)
    )
}

/// Accessible name for the countdown chip. A NOUN plus the value, never a verb —
/// the chip is a status, not a control, and nothing about it toggles.
pub fn compose_chip_label(remaining_ms: u64, urgency: TimerUrgency) -> String {
    if urgency == TimerUrgency::Expired {
        "Meeting timer: time is up".to_string()
    } else {
        format!(
            "Meeting timer: {} remaining",
            format_duration_words(remaining_ms)
        )
    }
}

/// Play the "time is up" sound for every participant.
///
/// Synthesized with the Web Audio API rather than loaded from a file. That is
/// the in-meeting convention here — join and leave are both synthesized two-tone
/// pairs in `attendants.rs` — and it means the feature ships with no new binary
/// asset, hence no third-party licence to audit and no 404 if the asset ever
/// fails to copy. (`/assets/knock.wav`, the app's only file-based sound, is a
/// waiting-room notification for the host alone; this one plays for the entire
/// room, so getting it wrong is louder.)
///
/// THREE equal, rapid tones at a single pitch, deliberately unlike either
/// existing cue: join rises (C5 -> E5), leave falls (E5 -> A4), and both are
/// two-tone. A flat repeated triplet is not mistakable for either, and repetition
/// reads as "alarm" across cultures where a specific melody may not.
///
/// The visual Expired state is authoritative on its own — this sound is an
/// enhancement, never the only signal, since it is inaudible to some users and
/// muted for others (browsers block audio until the page has been interacted
/// with, and a participant may simply have the tab muted).
pub fn play_timer_expired_sound() {
    const FREQ: f32 = 880.0; // A5
    const TONE_S: f64 = 0.14;
    const GAP_S: f64 = 0.09;
    const TONES: usize = 3;
    const VOLUME: f32 = 0.35;

    if web_sys::window().is_none() {
        return;
    }
    let ctx = match web_sys::AudioContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            log::warn!("Failed to create AudioContext for timer expiry sound: {e:?}");
            return;
        }
    };
    let now = ctx.current_time();

    for i in 0..TONES {
        let start = now + (i as f64) * (TONE_S + GAP_S);
        let (Ok(osc), Ok(gain)) = (ctx.create_oscillator(), ctx.create_gain()) else {
            log::warn!("Failed to build oscillator for timer expiry sound");
            continue;
        };
        let _ = osc.connect_with_audio_node(&gain);
        let _ = gain.connect_with_audio_node(&ctx.destination());
        osc.set_type(web_sys::OscillatorType::Triangle);
        let _ = osc.frequency().set_value_at_time(FREQ, start);
        let _ = gain.gain().set_value_at_time(VOLUME, start);
        // Exponential ramp cannot reach 0, hence the 0.01 floor -- same shaping
        // the join/leave cue uses, so the three sounds sit at a consistent level.
        let _ = gain
            .gain()
            .exponential_ramp_to_value_at_time(0.01, start + TONE_S);
        let _ = osc.start_with_when(start);
        let _ = osc.stop_with_when(start + TONE_S);
    }

    // Release the context once playback is done. This `forget` is SAFE and is not
    // the hazard the raise-hand work documented: the closure captures only the
    // AudioContext and writes NO signal, so there is no dropped-scope panic to
    // have. It must outlive this scope by design -- the sound continues after the
    // component that started it may have unmounted.
    let total_ms = ((TONES as f64) * (TONE_S + GAP_S) * 1000.0) as u32 + 100;
    gloo_timers::callback::Timeout::new(total_ms, move || {
        let _ = ctx.close();
    })
    .forget();
}

/// Shared, room-global timer state. Read by [`MeetingTimerChip`] ONLY — see the
/// module docs for why nothing else may subscribe to it.
#[derive(Clone, Copy)]
pub struct MeetingTimerCtx(pub Signal<Option<MeetingTimerState>>);

/// A countdown that is currently being driven, together with the identity of the
/// state that produced it.
///
/// The identity is carried alongside the sample rather than re-read from the
/// context signal inside the tick, so the expiry latch and the countdown can
/// never disagree about WHICH timer is running — re-reading the context would
/// leave a window where the sample belongs to the old timer and the identity to
/// the new one.
#[derive(Clone, Copy, PartialEq)]
struct ActiveCountdown {
    sample: CountdownSample,
    /// `(ends_at_ms, updated_at_ms)` — the identity of the state being counted.
    identity: (u64, u64),
}

/// The always-visible countdown, rendered for EVERY participant.
///
/// Takes no props and reads its own context, so a timer transition re-renders
/// this ~6-node component instead of the attendants RSX. SELF-GATING: it emits
/// zero element nodes when no timer is running, which matters because several
/// E2E specs address `#grid-container`'s children positionally.
#[component]
pub fn MeetingTimerChip() -> Element {
    let timer = use_context::<MeetingTimerCtx>();
    let state = (timer.0)();

    // Local to THIS component. Nothing outside this scope can subscribe to it,
    // which is what bounds the per-second tick's blast radius to these few nodes.
    let mut remaining_ms = use_signal(|| 0u64);
    // The once-at-receipt wall-clock sample. Held in a signal so the interval
    // closure reads the CURRENT sample rather than one captured when it was armed.
    let mut active = use_signal(|| None::<ActiveCountdown>);
    // Latches the expiry sound to the timer that earned it, so a heartbeat, a
    // transition repeat, or a re-render cannot make the room beep twice.
    //
    // `use_hook` + `Cell`, NOT `use_signal`: nothing renders from this, so it must
    // never dirty the component.
    let expiry_latch = use_hook(|| Rc::new(RefCell::new(ExpiryLatch::new())));

    // Re-sample whenever the wire state changes (rule 4: sample the wall clock
    // ONCE per state, then count down monotonically).
    let expiry_seed = expiry_latch.clone();
    use_effect(move || match (timer.0)() {
        Some(s) if s.running => {
            let now_mono = now_mono_ms();
            let sampled =
                CountdownSample::sample(s.ends_at_ms, js_sys::Date::now() as u64, now_mono);
            let next = ActiveCountdown {
                sample: sampled,
                identity: (s.ends_at_ms, s.updated_at_ms),
            };
            if active.peek().as_ref() != Some(&next) {
                active.set(Some(next));
            }
            let r = sampled.remaining_at(now_mono);
            if *remaining_ms.peek() != r {
                remaining_ms.set(r);
            }
            // ALREADY at zero the first time we see this state: SEED the expiry
            // latch instead of letting the first tick ring.
            //
            // Only a zero CROSSED WHILE WATCHING is an event. A zero that was
            // already true when the state arrived is history, and ringing for it
            // means every late joiner and every page reload hears the alarm about
            // a second after entering the meeting — for a timer that expired
            // minutes ago, in front of a room where nothing just happened.
            //
            // Reachable because the host keeps heartbeating past zero (bounded by
            // MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS, which SHRINKS this window
            // but cannot close it: inside the grace period the expired state is
            // still delivered, which is the point of delivering it).
            //
            // NOT covered by the reconnect drop: at connect a fresh client holds
            // `None`, so `should_drop_timer_on_connect` correctly declines, and
            // the expired state arrives afterwards on the heartbeat.
            //
            // Keyed on the identity, so a timer the host later EXTENDS past now
            // gets a fresh countdown and can ring properly when it really does
            // reach zero.
            expiry_seed.borrow_mut().observe_sample(next.identity, r);
        }
        _ => {
            if active.peek().is_some() {
                active.set(None);
            }
            if *remaining_ms.peek() != 0 {
                remaining_ms.set(0);
            }
        }
    });

    // The countdown tick, armed ONLY while a timer is actually running.
    //
    // Held in a signal, never `.forget()`-ed, so both unmount AND a cancel drop
    // it — dropping an `Interval` cancels it. Deliberately NOT a `use_hook`: that
    // runs before the self-gating early returns below, so it would leave EVERY
    // participant running a 1 Hz timer for the whole meeting even when no timer
    // is ever set. `MeetingTimerLiveRegion` already does it this way; this now
    // matches.
    let mut ticker: Signal<Option<Interval>> = use_signal(|| None);
    use_effect(move || {
        let running = (timer.0)().is_some_and(|s| s.running);
        if !running {
            if ticker.peek().is_some() {
                ticker.set(None);
            }
            return;
        }
        if ticker.peek().is_some() {
            // Already ticking. The closure reads `active` dynamically, so a
            // start/extend needs no re-arm — re-arming would only reset the
            // sub-second phase for no benefit.
            //
            // NOT disarmed once the countdown reaches zero, which is a CHOICE
            // rather than a constraint. After expiry `next` is pinned at 0, so
            // the closure does nothing but a peek-guarded comparison — no signal
            // write, no re-render — yet it does keep firing once a second until
            // the timer is cancelled or the meeting ends.
            //
            // A safe fix exists: this effect could read `remaining_ms()`
            // REACTIVELY instead of through `peek` and drop the `Interval` here,
            // from the effect, when it hits zero. What is NOT safe is dropping it
            // from inside the callback — `Interval::drop` calls `clearInterval`
            // and frees the boxed `Closure` that is still on the stack.
            //
            // Left as-is deliberately: one no-op closure per second, per
            // participant, only while a timer has actually run and only until it
            // is cleared, against making this effect re-run every second to
            // subscribe to the countdown it is meant to be insulated from.
            return;
        }
        let mut remaining_ms = remaining_ms;
        let active = active;
        let expiry_latch = expiry_latch.clone();
        let interval = Interval::new(MEETING_TIMER_TICK_MS, move || {
            // `try_peek` / `try_write` throughout: the interval is cancelled on
            // drop, but it can still fire once while the component is being torn
            // down, and `unwrap` there aborts the module and drops the call.
            let Ok(current) = active.try_peek().map(|a| *a) else {
                return;
            };
            let Some(current) = current else {
                return;
            };
            let next = current.sample.remaining_at(now_mono_ms());

            // Compare through `try_peek` and take the write guard ONLY when the
            // value actually moves. Testing inside the guard would not work: a
            // write guard marks the signal dirty when it is ACQUIRED, not when it
            // is assigned through, so `try_write()` followed by "and don't
            // assign" still schedules a re-render.
            //
            // This matters most AFTER expiry, where `next` stays pinned at 0: the
            // guard-first shape would re-render the chip once a second, forever,
            // for a countdown whose text never changes again.
            let unchanged = remaining_ms.try_peek().is_ok_and(|v| *v == next);
            if !unchanged {
                if let Ok(mut w) = remaining_ms.try_write() {
                    *w = next;
                }
            }

            // `ExpiryLatch` owns both halves of this decision -- the once-per-timer
            // guard AND the "only a zero we watched arrive" rule -- so the two
            // cannot drift apart across the two call sites that drive it.
            if expiry_latch
                .borrow_mut()
                .should_ring(current.identity, next)
            {
                play_timer_expired_sound();
            }
        });
        ticker.set(Some(interval));
    });

    let Some(state) = state else {
        return rsx! {};
    };
    if !state.running {
        return rsx! {};
    }

    let remaining = remaining_ms();
    let urgency = urgency(remaining, state.duration_ms);
    let label = compose_chip_label(remaining, urgency);

    // The chip is bottom-LEFT anchored, which is the only bottom corner that is
    // free: bottom-centre at the dock clearance belongs to the decode-paused
    // pill and the screen-share zoom controls, and bottom-right to the self-view
    // tile (which widens to 35% on mobile). A VERTICAL dock, though, occupies
    // exactly that corner's column, so the chip carries the dock position as a
    // modifier and the stylesheet shifts it clear.
    let dock_class = dock_position_class();

    rsx! {
        div {
            class: "meeting-timer-chip meeting-timer-chip--{urgency.modifier()} meeting-timer-chip--{dock_class}",
            "data-testid": "meeting-timer-chip",
            // Exposed for the E2E spec and for CSS, so neither has to infer the
            // state from a class name that also carries styling concerns.
            "data-urgency": urgency.modifier(),
            "data-remaining-ms": "{remaining}",
            // A STATUS, not a live region. The announcements are made by
            // `MeetingTimerLiveRegion` on a milestone cadence; marking this
            // element live as well would speak the countdown every second.
            role: "img",
            "aria-label": label,
            span { class: "meeting-timer-chip-icon", aria_hidden: true, "\u{23f1}" }
            span { class: "meeting-timer-chip-value", aria_hidden: true, "{format_remaining(remaining)}" }
        }
    }
}

/// `performance.now()` where available, falling back to the wall clock.
///
/// The fallback is deliberately the wall clock rather than a panic: without
/// `performance` the countdown loses its immunity to a mid-run clock step, which
/// is a degradation, not a failure. Refusing to render a timer at all would be
/// worse.
fn now_mono_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or_else(js_sys::Date::now)
}

/// The host's action-bar control, wrapped so the CONTEXT READ lands here.
///
/// [`MeetingTimerButton`] stays purely presentational — every input is a prop,
/// so it is renderable and assertable without a context — and this ~5-line
/// wrapper is the thing that subscribes to `MeetingTimerCtx` for the one field
/// the button derives from shared state (`running`, which selects the tooltip
/// wording and the `data-running` hook).
///
/// WHY A WRAPPER AT ALL. Reading the context in `AttendantsComponent`'s render
/// body to pass `running` down as a prop would subscribe that ~9,000-line RSX,
/// and every keyed `PeerTile` under it, to every timer transition — the #1296 /
/// #2103 blast-radius hazard. The cost would admittedly be small here (a
/// transition is a handful of events per meeting, not the ~5s heartbeat, which
/// [`would_apply_change`] already suppresses), but "small" is a judgement that
/// decays: the same reasoning is how the raised-hand roster ended up read in the
/// attendants body once. Scoping the subscription to a component that renders
/// one button makes the bound STRUCTURAL, and keeps the module's stated
/// invariant — that nothing outside this module subscribes to the timer — true
/// as written rather than true-with-an-asterisk.
#[component]
pub fn MeetingTimerDockControl(
    /// Whether the controls popover is open. Owned by the attendants component,
    /// not by the timer state, so it stays a prop.
    open: bool,
    /// DOM id for the trigger, so the popover can return focus to it on close
    /// (Escape, cancel, or starting a timer) rather than dropping focus to the
    /// document body.
    #[props(default)]
    id: String,
    #[props(default)] describedby: Option<&'static str>,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    // `try_use_context`: the customize-mode preview and any isolated component
    // test render this button without a provider, and a missing timer is simply
    // "not running" rather than a panic.
    let running = try_use_context::<MeetingTimerCtx>()
        .and_then(|ctx| (ctx.0)())
        .map(|s| s.running)
        .unwrap_or(false);
    rsx! {
        MeetingTimerButton { open, running, id, describedby, onclick }
    }
}

/// The active dock's CSS class (`dock-bottom` / `dock-left` / `dock-right`).
///
/// Both the chip and the controls popover are `position: fixed` and share no
/// positioned ancestor with the dock, so neither can anchor to it in CSS alone —
/// each carries the dock position as a modifier class instead and the stylesheet
/// places it. `DockPosition` is a stored user preference that changes at most a
/// handful of times per session, so subscribing to it costs nothing.
///
/// `try_use_context` with a bottom-dock fallback: the customize-mode preview and
/// any isolated component test render without a provider.
fn dock_position_class() -> &'static str {
    try_use_context::<DockPositionCtx>()
        .map(|ctx| (ctx.0)().css_class())
        .unwrap_or("dock-bottom")
}

/// DOM id of the custom-duration field, so its visible `<label>` can point at it
/// with `for` rather than relying on an `aria-label` alone (a real label is also
/// a click target that focuses the field).
const MEETING_TIMER_CUSTOM_VALUE_ID: &str = "meeting-timer-custom-value";

/// DOM id of the custom row's hint, referenced by the field's AND the confirm
/// button's `aria-describedby` so the rule, the resolved duration, and the
/// reason the button is inert are all read out with the control in hand.
const MEETING_TIMER_CUSTOM_HINT_ID: &str = "meeting-timer-custom-hint";

/// DOM id of the "Custom length" label, so the field+unit pair can be exposed as
/// one labelled `role="group"` rather than two unrelated controls.
const MEETING_TIMER_CUSTOM_LABEL_ID: &str = "meeting-timer-custom-label";

/// Accessible name for the custom Start button.
///
/// Spells the resulting duration out the way the presets do, because "Start"
/// alone names no duration at all. Split out of the RSX so the running branch of
/// [`MeetingTimerPopover`] never allocates it, and so the wording is unit-
/// testable rather than only reachable through a rendered component.
fn compose_custom_start_label(custom_ms: Option<u64>) -> String {
    match custom_ms {
        Some(ms) => format!("Start a {} timer", format_duration_words(ms)),
        None => "Start a custom timer".to_string(),
    }
}

/// Visible hint under the custom row, also its `aria-describedby` target.
///
/// Restates the RESOLVED duration, which is how a CLAMPED entry becomes visible:
/// type 2000 minutes and this reads "24 hours". When the entry does not parse it
/// states the rule instead, which is the only place a keyboard or screen-reader
/// user learns why Start is inert.
///
/// Deliberately NOT a live region — one that announced would speak on every
/// keystroke.
fn compose_custom_hint(custom_ms: Option<u64>) -> String {
    match custom_ms {
        Some(ms) => format!("Timer will run for {}.", format_duration_words(ms)),
        None => "Enter a whole number, then pick minutes or seconds.".to_string(),
    }
}

/// The host's start / extend / cancel panel (issue 2136).
///
/// A COMPONENT rather than inline RSX in `AttendantsComponent` for the same
/// reason as [`MeetingTimerDockControl`]: the running/idle branch has to read
/// `MeetingTimerCtx`, and reading it in the attendants body would subscribe that
/// whole RSX to every timer transition.
///
/// The caller owns whether this is mounted at all (open state AND the live host
/// gate) and supplies the four handlers; this component owns only the layout and
/// the running/idle choice.
#[component]
pub fn MeetingTimerPopover(
    on_start: EventHandler<u64>,
    on_extend: EventHandler<()>,
    on_cancel: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    let running = try_use_context::<MeetingTimerCtx>()
        .and_then(|ctx| (ctx.0)())
        .map(|s| s.running)
        .unwrap_or(false);
    // Anchored beside the bar when the dock is on a side -- this is where a host
    // extends a RUNNING timer under time pressure, so a long trip to find the
    // panel actually costs something here.
    let dock_class = dock_position_class();

    // Issue 2172: the typed duration. Local to this component, which the caller
    // mounts only while the popover is OPEN -- so the field starts empty on every
    // open and there is no stale entry from a previous session to guard against.
    let mut custom_value = use_signal(String::new);
    let mut custom_unit = use_signal(|| MEETING_TIMER_CUSTOM_DEFAULT_UNIT);
    let custom_raw = custom_value();
    let unit = custom_unit();
    // Both of these are allocation-free (a parse that short-circuits on the empty
    // string plus a comparison), so they cost nothing on the running branch that
    // does not use them. The two STRING derivations are not computed here for
    // exactly that reason -- see `compose_custom_start_label` / `_hint`, which are
    // called from inside the idle branch.
    let custom_ms = custom_duration_ms(&custom_raw, unit);

    // Shared by the confirm button and the Enter key so the two cannot diverge.
    // Reads through `peek` rather than closing over `custom_ms`: the render that
    // produced that value may not have flushed yet when a fast typist hits Enter,
    // and a stale capture would start the previous keystroke's duration.
    let submit_custom = move || {
        let parsed = custom_duration_ms(custom_value.peek().as_str(), *custom_unit.peek());
        // The peek guards are released by the end of the statement above, BEFORE
        // `on_start` runs -- it closes the popover, which unmounts this component
        // and drops these signals.
        if let Some(ms) = parsed {
            on_start.call(ms);
        }
    };

    rsx! {
        div {
            class: "meeting-timer-popover meeting-timer-popover--{dock_class}",
            id: "meeting-timer-popover",
            role: "dialog",
            "aria-label": "Meeting timer",
            "data-testid": "meeting-timer-popover",
            // Exposed so the E2E spec and CSS can read the branch without
            // inferring it from which buttons happen to be present.
            "data-running": if running { "true" } else { "false" },
            // Clicks inside must not reach `#main-container`'s background
            // light-dismiss handler (#1790).
            onclick: move |e: MouseEvent| e.stop_propagation(),
            onkeydown: move |evt: Event<KeyboardData>| {
                if evt.key() == Key::Escape {
                    evt.stop_propagation();
                    on_close.call(());
                }
            },

            if running {
                // RUNNING: the host's job here is to add time or stop. The presets
                // are HIDDEN rather than disabled — starting a fresh timer mid-run
                // is not a thing a host can want, and a row of dead buttons reads
                // as breakage rather than as a deliberate restriction.
                div { class: "meeting-timer-popover-row",
                    button {
                        class: "meeting-timer-popover-action",
                        r#type: "button",
                        "data-testid": "meeting-timer-extend",
                        onclick: move |_| on_extend.call(()),
                        "Add {format_duration_words(MEETING_TIMER_EXTEND_STEP_MS)}"
                    }
                    button {
                        class: "meeting-timer-popover-action meeting-timer-popover-action--cancel",
                        r#type: "button",
                        "data-testid": "meeting-timer-cancel",
                        onclick: move |_| on_cancel.call(()),
                        "Cancel timer"
                    }
                }
            } else {
                // IDLE: pick a duration. Each preset is its own button rather than a
                // select plus a confirm, so starting a timer is ONE click — the host
                // is usually doing this while someone is already talking.
                div { class: "meeting-timer-popover-row",
                    for preset_ms in MEETING_TIMER_PRESETS_MS.iter().copied() {
                        button {
                            key: "{preset_ms}",
                            class: "meeting-timer-popover-action",
                            r#type: "button",
                            "data-testid": "meeting-timer-preset-{preset_ms}",
                            // The visible text is an abbreviation ("5 min"); the
                            // accessible name spells it out, because most screen
                            // readers say "min" rather than "minutes".
                            "aria-label": "Start a {format_duration_words(preset_ms)} timer",
                            onclick: move |_| on_start.call(preset_ms),
                            "{preset_ms / 60_000} min"
                        }
                    }
                }

                // Issue 2172: anything the presets do not cover, down to a single
                // second. BELOW the presets rather than beside them -- the
                // one-click path stays the widest, most obvious target, and a
                // field sharing that row would squeeze all four presets under the
                // 44px touch target the row is built around.
                div { class: "meeting-timer-popover-custom",
                    label {
                        id: MEETING_TIMER_CUSTOM_LABEL_ID,
                        class: "meeting-timer-popover-custom-label",
                        r#for: MEETING_TIMER_CUSTOM_VALUE_ID,
                        "Custom length"
                    }
                    div { class: "meeting-timer-popover-row meeting-timer-popover-row--custom",
                        // The amount and its unit are ONE value split across two
                        // controls, so they are exposed as one labelled group
                        // rather than as two unrelated fields. The confirm button
                        // stays outside it -- it is an action, not part of the
                        // value.
                        div {
                            class: "meeting-timer-custom-group",
                            role: "group",
                            "aria-labelledby": MEETING_TIMER_CUSTOM_LABEL_ID,
                            input {
                                id: MEETING_TIMER_CUSTOM_VALUE_ID,
                                class: "meeting-timer-custom-value",
                                "data-testid": "meeting-timer-custom-value",
                                // TEXT, not `type="number"`, with the numeric soft
                                // keyboard kept via `inputmode`.
                                //
                                // A number input reports "" for any value that is
                                // not a valid floating-point number, and dioxus-html
                                // declares input `value` VOLATILE (elements.rs:1488),
                                // so it writes the signal back to the DOM every
                                // render instead of trusting the diff. Those two
                                // compose badly on the INTERMEDIATE states of an
                                // ordinary keystroke sequence: typing "1.5" passes
                                // through "1.", which is not a valid float, so the
                                // read is "", the signal becomes "", and the
                                // volatile write CLEARS the field -- the "5" then
                                // lands in an empty box and the button truthfully
                                // offers to start a 5-minute timer for an entry the
                                // host never made.
                                //
                                // Silently mangling the value is precisely what the
                                // parser exists to prevent, so the rejection has to
                                // stay VISIBLE: with a text input "1.5" stays on
                                // screen, Start stays inert, and the hint says why.
                                r#type: "text",
                                inputmode: "numeric",
                                // The field is a duration, not an identity -- a
                                // saved-value dropdown over it is noise.
                                autocomplete: "off",
                                "aria-describedby": MEETING_TIMER_CUSTOM_HINT_ID,
                                value: "{custom_raw}",
                                oninput: move |e: Event<FormData>| custom_value.set(e.value()),
                                onkeydown: move |evt: Event<KeyboardData>| {
                                    // Enter commits, exactly as clicking Start does. Escape
                                    // is deliberately NOT handled here so it keeps bubbling
                                    // to the dialog's own handler and still closes the
                                    // popover from inside the field.
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        submit_custom();
                                    }
                                },
                            }
                            select {
                                class: "meeting-timer-custom-unit",
                                "data-testid": "meeting-timer-custom-unit",
                                // A native select rather than a segmented radiogroup:
                                // it brings arrow keys, type-ahead and Home/End for
                                // free, which matters inside a dialog that already has
                                // a deliberate keyboard model to preserve.
                                "aria-label": "Custom length unit",
                                onchange: move |e: Event<FormData>| {
                                    if let Some(u) = CustomDurationUnit::from_value(&e.value()) {
                                        custom_unit.set(u);
                                    }
                                },
                                // The SAME Enter handler as the field. Without it,
                                // Tab-to-unit-then-Enter is a dead key -- the one
                                // path a keyboard user takes to change the unit and
                                // commit. Bound per-control rather than hoisted to
                                // the row, because a row-level handler would also
                                // see the Enter that ACTIVATES the Start button and
                                // submit twice.
                                onkeydown: move |evt: Event<KeyboardData>| {
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        submit_custom();
                                    }
                                },
                                option {
                                    value: CustomDurationUnit::Minutes.as_value(),
                                    selected: unit == CustomDurationUnit::Minutes,
                                    "minutes"
                                }
                                option {
                                    value: CustomDurationUnit::Seconds.as_value(),
                                    selected: unit == CustomDurationUnit::Seconds,
                                    "seconds"
                                }
                            }
                        }
                        button {
                            class: "meeting-timer-popover-action meeting-timer-popover-action--custom",
                            r#type: "button",
                            "data-testid": "meeting-timer-custom-start",
                            // `aria-disabled`, NOT the `disabled` attribute. A real
                            // `disabled` drops the button out of the tab order, so
                            // a keyboard or screen-reader user reaches the end of
                            // the popover having never met the control and is never
                            // told what it wants. This stays focusable and
                            // describes itself; `submit_custom` is a no-op on a
                            // `None` parse, so activating it anyway does nothing.
                            "aria-disabled": if custom_ms.is_none() { "true" } else { "false" },
                            // Spells the resulting duration out, like the presets do.
                            "aria-label": compose_custom_start_label(custom_ms),
                            // The reason it is inert, for someone who tabbed
                            // straight to it without passing through the field.
                            "aria-describedby": MEETING_TIMER_CUSTOM_HINT_ID,
                            onclick: move |_| submit_custom(),
                            "Start"
                        }
                    }
                    p {
                        id: MEETING_TIMER_CUSTOM_HINT_ID,
                        class: "meeting-timer-popover-hint",
                        "data-testid": "meeting-timer-custom-hint",
                        {compose_custom_hint(custom_ms)}
                    }
                }
            }
            p { class: "meeting-timer-popover-hint",
                "Everyone sees this countdown, and hears a sound when it reaches zero."
            }
        }
    }
}

/// Screen-reader announcements for the countdown, on a MILESTONE cadence.
///
/// Separate from the chip on purpose. The chip carries no live-region role at
/// all, so the countdown is never spoken every second — which would talk over
/// every other participant and every other live region on the page for the whole
/// life of the timer. This region speaks only at the instants a listener can act
/// on: the transitions, then 5 min / 1 min / 30 s / 10 s / time's up.
///
/// `polite`, including for expiry. `assertive` interrupts whatever the user is
/// currently being read, and expiry already has an audible cue of its own plus a
/// permanent visual state — so the marginal value of interrupting is low and the
/// cost (cutting off a sentence about something else) is real.
#[component]
pub fn MeetingTimerLiveRegion() -> Element {
    let timer = use_context::<MeetingTimerCtx>();
    let mut announcement = use_signal(String::new);
    // Last remaining value this region has SEEN, so milestone crossings can be
    // detected across a coalesced background tick. `use_hook` + `Cell`: nothing
    // renders from it.
    let last_remaining = use_hook(|| Rc::new(Cell::new(u64::MAX)));
    let last_identity = use_hook(|| Rc::new(Cell::new(None::<TimerIdentity>)));
    // The identity we last SPOKE, which outlives a drop/re-acquire cycle.
    let last_announced = use_hook(|| Rc::new(Cell::new(None::<TimerIdentity>)));
    let mut poll_timer = use_signal(|| None::<Interval>);

    // Announce TRANSITIONS from the shared state.
    let transition_last_remaining = last_remaining.clone();
    use_effect(move || {
        let last_remaining = &transition_last_remaining;
        let state = (timer.0)();
        let identity = state.map(|s| (s.ends_at_ms, s.updated_at_ms));
        if last_identity.get() == identity {
            return;
        }
        last_identity.set(identity);
        // SUPPRESS a re-announcement of a timer we have already announced.
        //
        // A viewer whose connection flaps DROPS its timer on every reconnect (see
        // `should_drop_timer_on_connect`) and re-acquires it on the next
        // heartbeat, which without this would utter "Meeting timer set. N
        // remaining." once per flap — a screen reader talking over the meeting
        // every few seconds on a bad link, about a timer that never changed.
        //
        // Keyed on the identity we last SPOKE, not the one we last saw, so the
        // drop itself does not clear the memory. A genuinely new timer (different
        // ends_at/updated_at) still announces.
        if identity.is_some() && identity == last_announced.get() {
            return;
        }
        if identity.is_some() {
            last_announced.set(identity);
        }
        match state {
            Some(s) if s.running => {
                let remaining = s.ends_at_ms.saturating_sub(js_sys::Date::now() as u64);
                last_remaining.set(remaining);
                announcement.set(compose_transition_announcement(s, remaining));
            }
            Some(s) => {
                last_remaining.set(u64::MAX);
                announcement.set(compose_transition_announcement(s, 0));
            }
            None => {
                last_remaining.set(u64::MAX);
                if !announcement.peek().is_empty() {
                    announcement.set(String::new());
                }
            }
        }
    });

    // Poll for milestone crossings. A dedicated interval rather than a
    // subscription to the chip's countdown: the two have different cadences (the
    // chip repaints every second, this speaks a handful of times per timer), and
    // coupling them would either make the chip announce or make this re-render at
    // 1 Hz for nothing.
    let poll_last_remaining = last_remaining.clone();
    use_effect(move || {
        let last_remaining = &poll_last_remaining;
        let state = (timer.0)();
        let Some(s) = state.filter(|s| s.running) else {
            // Dropping the Interval here CANCELS it -- a stopped timer must not
            // leave a poll running for the rest of the meeting.
            if poll_timer.peek().is_some() {
                poll_timer.set(None);
            }
            return;
        };
        let ends_at_ms = s.ends_at_ms;
        // Needed by `milestone_crossed`, which drops the 10s call on a short timer.
        let duration_ms = s.duration_ms;
        let last_remaining = last_remaining.clone();
        let mut announcement = announcement;
        let interval = Interval::new(MEETING_TIMER_TICK_MS, move || {
            let remaining = ends_at_ms.saturating_sub(js_sys::Date::now() as u64);
            let prev = last_remaining.get();
            last_remaining.set(remaining);
            if let Some(m) = milestone_crossed(prev, remaining, duration_ms) {
                // `try_write`, not `set`: this interval is cancelled on drop but
                // can still fire during teardown, where `set` would panic.
                if let Ok(mut w) = announcement.try_write() {
                    *w = compose_milestone_announcement(m);
                }
            }
        });
        // Held in a signal so it is cancelled on drop -- never `.forget()`ed.
        poll_timer.set(Some(interval));
    });

    let text = announcement();
    rsx! {
        div {
            class: "visually-hidden",
            "data-testid": "meeting-timer-live-region",
            role: "status",
            // `polite` and NOT `off`. A `role="status"` with `aria-live="off"` is
            // self-cancelling -- the role sets up a live region and the attribute
            // then switches it off, so nothing is ever announced.
            "aria-live": "polite",
            "aria-atomic": "true",
            "{text}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The 24h ceiling is referenced only by tests now that the field carries no
    // `max` attribute — `custom_duration_ms` reaches it through
    // `clamp_duration_ms`, not by name.
    use videocall_client::MEETING_TIMER_MAX_DURATION_MS;

    fn st(running: bool, ends: u64, dur: u64, updated: u64) -> MeetingTimerState {
        MeetingTimerState {
            running,
            ends_at_ms: ends,
            duration_ms: dur,
            updated_at_ms: updated,
        }
    }

    // ---------------------------------------------------------------------
    // format_remaining
    // ---------------------------------------------------------------------

    #[test]
    fn remaining_is_formatted_minutes_and_seconds() {
        assert_eq!(format_remaining(0), "0:00");
        assert_eq!(format_remaining(1_000), "0:01");
        assert_eq!(format_remaining(59_000), "0:59");
        assert_eq!(format_remaining(60_000), "1:00");
        assert_eq!(format_remaining(600_000), "10:00");
    }

    #[test]
    fn remaining_grows_an_hours_field_past_one_hour() {
        assert_eq!(format_remaining(3_600_000), "1:00:00");
        assert_eq!(format_remaining(3_661_000), "1:01:01");
    }

    /// Rounding UP is what keeps `0:00` meaning "expired" and nothing else. With
    /// truncation, the last 999ms of every timer would read `0:00` while the
    /// countdown was still running and no sound had played — the display would
    /// claim time was up a second before it was.
    #[test]
    fn remaining_rounds_up_so_zero_means_only_expired() {
        assert_eq!(
            format_remaining(1),
            "0:01",
            "any remaining time at all must render as at least one second"
        );
        assert_eq!(format_remaining(999), "0:01");
        assert_eq!(format_remaining(59_999), "1:00");
        assert_eq!(
            format_remaining(0),
            "0:00",
            "0:00 is reserved for the expired state, which is when the room hears a sound"
        );
    }

    // ---------------------------------------------------------------------
    // urgency
    // ---------------------------------------------------------------------

    #[test]
    fn urgency_is_expired_only_at_zero() {
        assert_eq!(urgency(0, 300_000), TimerUrgency::Expired);
        assert_eq!(urgency(1, 300_000), TimerUrgency::Critical);
    }

    #[test]
    fn urgency_is_critical_in_the_last_thirty_seconds_of_a_long_timer() {
        assert_eq!(
            urgency(MEETING_TIMER_CRITICAL_MS, 600_000),
            TimerUrgency::Critical
        );
        assert_eq!(
            urgency(MEETING_TIMER_CRITICAL_MS + 1, 600_000),
            TimerUrgency::Warning
        );
    }

    /// EVERY shipped preset must be able to render all three live states. The
    /// 60s preset could not before issue 2136's threshold fix: the flat
    /// `remaining <= MEETING_TIMER_WARNING_MS` floor is satisfied at t=0 for a
    /// 60s timer (`60_000 <= 120_000`), so it started amber and one of the three
    /// states was unreachable on the shortest preset we ship.
    ///
    /// MUTATION PROOF: restore the flat floor
    /// (`remaining_ms <= MEETING_TIMER_WARNING_MS || remaining_ms <= duration_ms / 4`)
    /// and the 60_000 case fails on its Normal assertion.
    #[test]
    fn every_shipped_preset_can_render_normal_warning_and_critical() {
        for &d in MEETING_TIMER_PRESETS_MS {
            let states: Vec<TimerUrgency> =
                (1..=d / 1_000).map(|s| urgency(s * 1_000, d)).collect();
            for want in [
                TimerUrgency::Normal,
                TimerUrgency::Warning,
                TimerUrgency::Critical,
            ] {
                assert!(
                    states.contains(&want),
                    "preset {d}ms never renders {want:?}; a preset that skips a \
                     visual state makes the colour ramp meaningless for it"
                );
            }
        }
    }

    /// The exact boundaries for each shipped preset, pinned so a future tweak to
    /// either threshold has to state which presets it moves.
    #[test]
    fn preset_urgency_boundaries_are_pinned() {
        // (duration, warn_at, crit_at) -- warn/crit are the LARGEST remaining
        // that still reads Warning / Critical.
        for (d, warn_at, crit_at) in [
            (60_000u64, 30_000u64, 15_000u64),
            (300_000, 120_000, 30_000),
            // Warning tracks duration/4 once it exceeds the flat 120s.
            (600_000, 150_000, 30_000),
            (900_000, 225_000, 30_000),
        ] {
            assert_eq!(urgency(crit_at, d), TimerUrgency::Critical, "crit at {d}");
            assert_eq!(
                urgency(crit_at + 1_000, d),
                TimerUrgency::Warning,
                "just above crit at {d}"
            );
            assert_eq!(urgency(warn_at, d), TimerUrgency::Warning, "warn at {d}");
            assert_eq!(
                urgency(warn_at + 1_000, d),
                TimerUrgency::Normal,
                "just above warn at {d}"
            );
        }
    }

    /// A SHORT timer gets its thresholds from the PROPORTIONAL cap, which is what
    /// keeps Normal reachable on it at all.
    #[test]
    fn a_short_timer_takes_the_proportional_thresholds() {
        let two_min = 120_000;
        // warn = min(120_000, 60_000).max(30_000) = 60_000; crit = min(30_000, 30_000) = 30_000.
        assert_eq!(urgency(90_000, two_min), TimerUrgency::Normal);
        assert_eq!(urgency(60_000, two_min), TimerUrgency::Warning);
        assert_eq!(urgency(30_000, two_min), TimerUrgency::Critical);
    }

    /// A LONG timer keeps the FLAT critical threshold -- a proportional critical
    /// on 45 minutes would be 11 minutes of red, which stops reading as urgent.
    #[test]
    fn a_long_timer_keeps_the_flat_critical_threshold() {
        let forty_five_min = 2_700_000;
        // 45 min: crit = min(30s, 675s) = 30s (the FLAT value -- a proportional
        // critical would be 11 minutes, which stops reading as urgent).
        // warn = min(120s, 1350s).max(675s) = 675s.
        assert_eq!(urgency(700_000, forty_five_min), TimerUrgency::Normal);
        assert_eq!(urgency(675_000, forty_five_min), TimerUrgency::Warning);
        assert_eq!(
            urgency(MEETING_TIMER_CRITICAL_MS, forty_five_min),
            TimerUrgency::Critical
        );
    }

    /// The proportional term must be written as `remaining <= duration / 4`, never
    /// `remaining * 4 <= duration`: the multiplication overflows.
    ///
    /// The witness value matters. `u64::MAX` does NOT distinguish the two forms —
    /// `u64::MAX.wrapping_mul(4)` is still enormous, so both spellings answer
    /// Normal and the test passes on the broken code. `1 << 62` is the value that
    /// bites: times four it is exactly 2^64, which wraps to ZERO, and `0 <=
    /// duration` is true — so the overflowing form reports WARNING for a timer
    /// with essentially forever left. (Found by mutation testing: the original
    /// `u64::MAX` version of this test stayed green against the wrapping form.)
    #[test]
    fn urgency_does_not_overflow_on_a_huge_remaining() {
        assert_eq!(
            urgency(1u64 << 62, 600_000),
            TimerUrgency::Normal,
            "a remaining time of 2^62 ms is nowhere near a 10-minute timer's last \
             quarter; the multiplying form wraps this to 0 and reports Warning"
        );
        assert_eq!(urgency(u64::MAX, 600_000), TimerUrgency::Normal);
    }

    #[test]
    fn urgency_handles_a_zero_duration_without_dividing_trouble() {
        assert_eq!(urgency(500_000, 0), TimerUrgency::Normal);
    }

    // ---------------------------------------------------------------------
    // would_apply_change -- the heartbeat de-dirtying guard
    // ---------------------------------------------------------------------

    /// THE re-render test. The host re-sends the running state every ~5s; if that
    /// wrote the shared signal, every subscriber would re-render 12 times a
    /// minute for the whole life of the timer.
    #[test]
    fn a_heartbeat_of_the_applied_state_is_not_written() {
        let applied = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        assert!(
            !would_apply_change(Some(applied), applied),
            "a heartbeat carries the state we already hold and must NOT dirty the \
             shared signal -- this is what keeps a 5s re-announce from becoming a \
             5s re-render of everything subscribed to the timer"
        );
    }

    #[test]
    fn a_genuine_transition_is_written() {
        let applied = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        let cancelled = MeetingTimerState::cleared(1_700_000_010_000);
        assert!(would_apply_change(Some(applied), cancelled));
    }

    #[test]
    fn the_first_state_is_written() {
        assert!(would_apply_change(None, st(true, 1_000, 500, 1)));
    }

    /// The guard must not swallow the ORDERING rule underneath it: a reordered
    /// START arriving after a CANCEL is both "different from applied" and stale,
    /// and it must still be refused.
    #[test]
    fn a_reordered_stale_state_is_not_written_even_though_it_differs() {
        let applied = MeetingTimerState::cleared(1_700_000_020_000);
        let stale_start = st(true, 1_700_000_300_000, 300_000, 1_700_000_019_000);
        assert!(
            !would_apply_change(Some(applied), stale_start),
            "differing from the applied state is not sufficient -- the bounded LWW \
             rule must still suppress a reordered packet, or a stale START \
             resurrects a timer the host already cancelled"
        );
    }

    // ---------------------------------------------------------------------
    // start_state / extend_state
    // ---------------------------------------------------------------------

    // ---------------------------------------------------------------------
    // the expiry latch: only a zero we WATCHED arrive may ring
    // ---------------------------------------------------------------------

    /// THE regression test for the late-joiner / page-reload alarm. The host keeps
    /// heartbeating an expired timer for a grace period, so a client that mounts
    /// during that window samples a state whose countdown is ALREADY zero. Ringing
    /// for it means every late joiner and every reload hears the alarm about a
    /// second after entering, about a timer that ran out before they arrived.
    ///
    /// MUTATION PROOF: make `observe_sample` a no-op and this goes red.
    #[test]
    fn a_zero_that_was_already_history_at_first_sample_never_rings() {
        let mut latch = ExpiryLatch::new();
        let id = (1_700_000_300_000u64, 1_700_000_000_000u64);
        // Mount into an already-expired timer.
        latch.observe_sample(id, 0);
        assert!(
            !latch.should_ring(id, 0),
            "a zero that was already true when we first saw the state is HISTORY, \
             not an event -- ringing for it alarms every late joiner and every \
             page reload about a timer that expired before they arrived"
        );
        assert!(
            !latch.should_ring(id, 0),
            "and it stays silent on later ticks"
        );
    }

    /// A zero CROSSED while watching must ring, exactly once.
    ///
    /// MUTATION PROOF: make `should_ring` return `false` and the first assertion
    /// goes red; drop the latch write inside it and the second does.
    #[test]
    fn a_zero_crossed_while_watching_rings_exactly_once() {
        let mut latch = ExpiryLatch::new();
        let id = (1_700_000_300_000u64, 1_700_000_000_000u64);
        latch.observe_sample(id, 5_000); // still running when we first saw it
        assert!(!latch.should_ring(id, 3_000), "not yet");
        assert!(latch.should_ring(id, 0), "the crossing must ring");
        assert!(
            !latch.should_ring(id, 0),
            "and must not ring again on every subsequent tick"
        );
    }

    /// A timer the host EXTENDS past now is a new identity, so it can ring again
    /// when it really does reach zero — no flag has to remember to reset.
    #[test]
    fn extending_an_expired_timer_lets_it_ring_again() {
        let mut latch = ExpiryLatch::new();
        let first = (1_700_000_300_000u64, 1_700_000_000_000u64);
        latch.observe_sample(first, 1_000);
        assert!(latch.should_ring(first, 0));

        // Host extends: new ends_at + new updated_at.
        let extended = (1_700_000_360_000u64, 1_700_000_300_000u64);
        latch.observe_sample(extended, 60_000);
        assert!(!latch.should_ring(extended, 30_000));
        assert!(
            latch.should_ring(extended, 0),
            "an extended timer is a NEW timer and must be able to ring on its own \
             expiry"
        );
    }

    /// A heartbeat re-samples the SAME state repeatedly. Re-observing a running
    /// sample must not disarm a latch, and must not arm one either.
    #[test]
    fn re_observing_a_running_sample_does_not_change_the_latch() {
        let mut latch = ExpiryLatch::new();
        let id = (1_700_000_300_000u64, 1_700_000_000_000u64);
        latch.observe_sample(id, 5_000);
        latch.observe_sample(id, 4_000);
        latch.observe_sample(id, 3_000);
        assert!(latch.should_ring(id, 0), "the crossing must still ring");
    }

    // ---------------------------------------------------------------------
    // reconnect: drop a timer we are only a VIEWER of
    // ---------------------------------------------------------------------

    /// THE regression test for the missed-cancel hole. A viewer that was
    /// disconnected across the host's 3-packet cancel burst holds a running timer
    /// nobody will ever correct — `running = false` has no heartbeat behind it —
    /// and would count it to zero and play the expiry sound at a room where
    /// nothing expired.
    ///
    /// MUTATION PROOF: return `false` unconditionally (i.e. never drop, the
    /// behaviour before this fix) and this goes red.
    #[test]
    fn a_viewer_drops_its_timer_on_reconnect_so_a_missed_cancel_cannot_beep() {
        let stale = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        assert!(
            should_drop_timer_on_connect(None, Some(stale)),
            "a client that is not driving a timer must drop what it holds on \
             (re)connect and wait for a heartbeat -- otherwise a cancel missed \
             while offline expires audibly at nobody"
        );
    }

    /// The HOST must keep its own timer across a reconnect. The relay self-skips
    /// the sender, so the host never receives its own packet back: its chip is a
    /// local echo with nothing to re-establish it, and dropping would blank the
    /// host's own countdown until it touched the controls again.
    ///
    /// MUTATION PROOF: return `true` whenever `applied.is_some()` (i.e. drop
    /// unconditionally) and this goes red.
    #[test]
    fn the_host_keeps_the_timer_it_is_driving_across_a_reconnect() {
        let running = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        assert!(
            !should_drop_timer_on_connect(Some(running), Some(running)),
            "the host is the only source of its own echo; dropping it would blank \
             the host's countdown with nothing able to bring it back"
        );
    }

    /// A host that has CANCELLED is no longer driving anything, so it is treated
    /// like any other client. Dropping is a no-op in display terms (both a
    /// cleared state and `None` render nothing) and keeps the rule single-branch.
    #[test]
    fn a_host_that_already_cancelled_is_not_treated_as_driving() {
        let cancelled = MeetingTimerState::cleared(1_700_000_010_000);
        assert!(should_drop_timer_on_connect(
            Some(cancelled),
            Some(cancelled)
        ));
    }

    /// Nothing held means nothing to drop -- the common case for every
    /// participant on every connect, and it must not churn the signal.
    #[test]
    fn a_connect_with_no_timer_held_is_a_no_op() {
        assert!(!should_drop_timer_on_connect(None, None));
        let running = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        assert!(!should_drop_timer_on_connect(Some(running), None));
    }

    #[test]
    fn start_sets_end_duration_and_stamp_consistently() {
        let now = 1_700_000_000_000;
        let s = start_state(300_000, now);
        assert!(s.running);
        assert_eq!(s.ends_at_ms, now + 300_000);
        assert_eq!(s.duration_ms, 300_000);
        assert_eq!(s.updated_at_ms, now);
        assert_eq!(s.started_at_ms(), now);
    }

    /// An over-cap duration must be clamped CLIENT-side. The relay DROPS an
    /// over-cap packet rather than clamping it, so an unclamped start would leave
    /// the host looking at a local echo of a timer no other participant can see.
    #[test]
    fn start_clamps_an_over_cap_duration_so_the_relay_cannot_drop_it() {
        use videocall_client::MEETING_TIMER_MAX_DURATION_MS;
        let s = start_state(u64::MAX, 1_700_000_000_000);
        assert_eq!(s.duration_ms, MEETING_TIMER_MAX_DURATION_MS);
    }

    #[test]
    fn extend_reports_the_total_span_not_the_increment() {
        let now = 1_700_000_000_000;
        let started = start_state(300_000, now);
        let extended = extend_state(started, 180_000, now + 60_000);
        assert_eq!(
            extended.duration_ms, 480_000,
            "the proto pins duration_ms as the TOTAL including the extension -- 5 \
             minutes extended by 3 is 8, not 3"
        );
        assert_eq!(
            extended.started_at_ms(),
            now,
            "the original start instant must survive the extension, or the rendered \
             progress proportion jumps"
        );
        assert_eq!(extended.ends_at_ms, now + 480_000);
        assert_eq!(extended.updated_at_ms, now + 60_000);
    }

    /// Extending an ALREADY-EXPIRED timer must give the presenter the full extra
    /// time from now. Extending from the stale end instant would produce a
    /// still-expired timer and an apparently dead button.
    #[test]
    fn extend_of_an_expired_timer_measures_from_now() {
        let now = 1_700_000_000_000;
        let started = start_state(60_000, now);
        // Two minutes after it expired.
        let later = now + 180_000;
        let extended = extend_state(started, MEETING_TIMER_EXTEND_STEP_MS, later);
        assert_eq!(
            extended.ends_at_ms,
            later + MEETING_TIMER_EXTEND_STEP_MS,
            "a host clicking +1 min after the buzzer means one more minute FROM NOW"
        );
        assert!(extended.ends_at_ms > later);
    }

    /// The relay rejects `ends_at_ms < duration_ms` at ingress, and any peer on an
    /// older relay underflows `started_at_ms` on it. Clamping the total and
    /// re-deriving the end instant from it keeps the two consistent by
    /// construction.
    #[test]
    fn extend_keeps_ends_at_and_duration_mutually_consistent_when_clamped() {
        use videocall_client::MEETING_TIMER_MAX_DURATION_MS;
        let now = 1_700_000_000_000;
        let started = start_state(MEETING_TIMER_MAX_DURATION_MS, now);
        let extended = extend_state(started, 600_000, now + 1_000);
        assert_eq!(extended.duration_ms, MEETING_TIMER_MAX_DURATION_MS);
        assert!(
            extended.ends_at_ms >= extended.duration_ms,
            "ends_at_ms must never fall below duration_ms -- the relay drops that \
             packet and it underflows started_at_ms on an older peer"
        );
        assert_eq!(extended.started_at_ms(), now);
    }

    // ---------------------------------------------------------------------
    // milestone announcements
    // ---------------------------------------------------------------------

    #[test]
    fn each_milestone_fires_once_when_crossed() {
        assert_eq!(milestone_crossed(61_000, 60_000, 300_000), Some(60_000));
        assert_eq!(milestone_crossed(60_000, 59_000, 300_000), None);
        assert_eq!(milestone_crossed(31_000, 29_000, 300_000), Some(30_000));
        // Spanning two adjacent milestones reports the lower one.
        assert_eq!(milestone_crossed(31_000, 9_000, 300_000), Some(10_000));
        assert_eq!(milestone_crossed(1_000, 0, 300_000), Some(0));
    }

    /// A background tab coalesces timers, so a tick can skip several seconds and
    /// span several milestones. The crossing test must fire rather than requiring
    /// an exact landing, and it must report the LOWEST milestone spanned.
    #[test]
    fn a_coalesced_background_tick_reports_the_lowest_milestone_it_spanned() {
        assert_eq!(
            milestone_crossed(65_000, 25_000, 300_000),
            Some(30_000),
            "a tab throttled for 40s spans BOTH the 60s and the 30s milestone; the \
             lowest is the one that is still true, and announcing the highest \
             would say '1 minute remaining' when 25s are left"
        );
    }

    /// THE case this ordering exists for. A tab throttled to ~1 tick/min takes a
    /// 60s timer from 60_000 straight to 0. Reporting the highest crossed
    /// milestone would announce "30 seconds remaining" and then never cross
    /// anything again (`prev == remaining == 0` forever), so the room's screen
    /// reader users would never be told the timer expired — on exactly the timers
    /// whose expiry matters most.
    ///
    /// MUTATION PROOF: swap `rfind()` for `find()` and this returns
    /// `Some(30_000)` instead of `Some(0)`.
    #[test]
    fn a_tick_that_spans_everything_still_announces_time_is_up() {
        assert_eq!(
            milestone_crossed(60_000, 0, 300_000),
            Some(0),
            "'Time is up.' must survive a throttled tab -- it is the one \
             announcement that cannot be inferred from the others"
        );
        assert_eq!(compose_milestone_announcement(0), "Time is up.");
    }

    #[test]
    fn no_milestone_is_reported_when_none_is_crossed() {
        assert_eq!(milestone_crossed(500_000, 400_000, 900_000), None);
        assert_eq!(milestone_crossed(0, 0, 300_000), None);
    }

    /// The 10-second call is dropped on a timer of a minute or less, so the 60s
    /// preset speaks at 30s and zero only — exactly its Warning and Expired
    /// transitions — instead of three times inside the final thirty seconds.
    ///
    /// MUTATION PROOF: make `announces_milestone` return `true` unconditionally
    /// and the first assertion goes red; make it return `false` for 10_000 at
    /// EVERY duration and the 5-minute assertion goes red.
    #[test]
    fn the_ten_second_call_is_dropped_on_a_one_minute_timer_only() {
        assert_eq!(
            milestone_crossed(12_000, 9_000, 60_000),
            None,
            "a 60s timer already changes colour at 30s and 15s; a 10s utterance \
             matches neither and lands five seconds after a visual change that \
             had no speech"
        );
        assert_eq!(
            milestone_crossed(12_000, 9_000, 300_000),
            Some(10_000),
            "on a longer timer the 10s call is a useful last warning, not noise"
        );
        // The boundary is inclusive at 60s and the guard touches nothing else.
        assert_eq!(milestone_crossed(12_000, 9_000, 61_000), Some(10_000));
        assert_eq!(milestone_crossed(31_000, 29_000, 60_000), Some(30_000));
        assert_eq!(milestone_crossed(1_000, 0, 60_000), Some(0));
    }

    #[test]
    fn milestone_text_reads_as_a_sentence() {
        assert_eq!(compose_milestone_announcement(0), "Time is up.");
        assert_eq!(
            compose_milestone_announcement(60_000),
            "1 minute remaining."
        );
        assert_eq!(
            compose_milestone_announcement(300_000),
            "5 minutes remaining."
        );
        assert_eq!(
            compose_milestone_announcement(30_000),
            "30 seconds remaining."
        );
    }

    /// All THREE branches, not two.
    ///
    /// The expired case is the one that was missing, and its absence was not
    /// theoretical: an already-expired timer is `running == true` with zero
    /// remaining, so it fell through to the "set" branch and told a screen-reader
    /// user "Meeting timer set. 0 seconds remaining." — present tense, as though
    /// a timer had just begun, and never that it had expired. Every late joiner
    /// and every page reload inside the heartbeat grace window hit it.
    ///
    /// This test previously covered only cancel and with-time-remaining, which is
    /// exactly why the gap survived review.
    ///
    /// MUTATION PROOF: delete the `remaining_ms == 0` branch and the expired case
    /// returns "Meeting timer set. 0 seconds remaining." -> red.
    #[test]
    fn transition_text_distinguishes_cancel_from_start_from_already_expired() {
        let s = start_state(300_000, 1_700_000_000_000);
        assert_eq!(
            compose_transition_announcement(s, 300_000),
            "Meeting timer set. 5 minutes remaining."
        );
        assert_eq!(
            compose_transition_announcement(MeetingTimerState::cleared(1), 0),
            "Meeting timer cancelled."
        );
        // ARRIVING AT AN ALREADY-EXPIRED TIMER. `running` is still true (nothing
        // clears it at zero), so only the remaining time distinguishes this from
        // a fresh start.
        assert_eq!(
            compose_transition_announcement(s, 0),
            "Meeting timer: time is up.",
            "an already-expired timer must NOT be announced as one that just \
             started -- the milestone path cannot rescue this, because it needs a \
             CROSSING and `last_remaining` is already 0 on arrival"
        );
    }

    /// The expired branch must not swallow a CANCEL, which also arrives with zero
    /// remaining. `running` is the discriminator and it has to be checked first.
    #[test]
    fn a_cancel_still_reads_as_cancelled_not_as_expired() {
        assert_eq!(
            compose_transition_announcement(MeetingTimerState::cleared(1), 0),
            "Meeting timer cancelled.",
            "a cancelled timer and an expired one both have zero remaining; only \
             `running` tells them apart, and confusing them would tell the room a \
             timer expired when the host called it off"
        );
    }

    // ---------------------------------------------------------------------
    // chip label
    // ---------------------------------------------------------------------

    /// The chip is a STATUS. Its accessible name must be a stable noun plus the
    /// value — never a verb, and never something that flips meaning with state,
    /// which is the bug pattern that shipped an `aria-pressed` toggle announcing
    /// the inverse of reality.
    #[test]
    fn chip_label_is_a_noun_plus_the_value() {
        assert_eq!(
            compose_chip_label(300_000, TimerUrgency::Normal),
            "Meeting timer: 5 minutes remaining"
        );
        assert_eq!(
            compose_chip_label(0, TimerUrgency::Expired),
            "Meeting timer: time is up"
        );
        for u in [
            TimerUrgency::Normal,
            TimerUrgency::Warning,
            TimerUrgency::Critical,
        ] {
            assert!(
                compose_chip_label(45_000, u).starts_with("Meeting timer:"),
                "the accessible name must not change shape with the visual urgency"
            );
        }
    }

    #[test]
    fn duration_words_are_pluralized() {
        assert_eq!(format_duration_words(60_000), "1 minute");
        assert_eq!(format_duration_words(120_000), "2 minutes");
        assert_eq!(format_duration_words(1_000), "1 second");
        assert_eq!(format_duration_words(30_000), "30 seconds");
        assert_eq!(format_duration_words(90_000), "1 minute 30 seconds");
    }

    /// Past an hour the wording grows an HOURS field. Issue 2172 is what makes
    /// this reachable: typing a duration puts the whole range up to the 24h cap
    /// one keystroke away, where before the longest preset was 15 minutes.
    ///
    /// MUTATION PROOF, both shapes the mistake takes, both actually run:
    ///
    ///  * DELETE the `hours` branch outright — one hour renders "0 seconds", and
    ///    so does the 24h cap. NOT "1440 minutes": `minutes` is
    ///    `(total_secs % 3_600) / 60`, which is 0 at a whole number of hours, so
    ///    the hours do not spill into it, they simply vanish.
    ///  * FOLD hours into minutes (`minutes + hours * 60`) — one hour renders
    ///    "60 minutes" and the cap renders "1440 minutes".
    ///
    /// Both go red on the FIRST assertion, since 3_600_000 already separates all
    /// three spellings.
    #[test]
    fn duration_words_grow_an_hours_field_past_an_hour() {
        assert_eq!(format_duration_words(3_600_000), "1 hour");
        assert_eq!(format_duration_words(7_200_000), "2 hours");
        assert_eq!(format_duration_words(3_660_000), "1 hour 1 minute");
        assert_eq!(
            format_duration_words(5_430_000),
            "1 hour 30 minutes 30 seconds"
        );
        assert_eq!(
            format_duration_words(MEETING_TIMER_MAX_DURATION_MS),
            "24 hours",
            "the ceiling a typed duration clamps to must read as hours -- \
             'Start a 1440 minutes timer' is not an accessible name anyone can \
             parse"
        );
    }

    /// Zero must still render, and as SECONDS. It is the wording
    /// `compose_transition_announcement` and `compose_chip_label` fall back to,
    /// and an empty string mid-sentence reads as a bug.
    ///
    /// MUTATION PROOF: drop the `|| parts.is_empty()` term and this returns "".
    #[test]
    fn a_zero_duration_still_reads_as_seconds() {
        assert_eq!(format_duration_words(0), "0 seconds");
    }

    // ---------------------------------------------------------------------
    // custom_duration_ms -- the typed duration (issue 2172)
    // ---------------------------------------------------------------------

    /// THE case the issue names: "such as 30 seconds". Asserted end to end
    /// through the production formatter as well, because that string is the
    /// confirm button's accessible name.
    ///
    /// MUTATION PROOF: change `CustomDurationUnit::Seconds.ms()` from 1_000 and
    /// both assertions go red.
    #[test]
    fn a_thirty_second_timer_is_typeable() {
        let ms = custom_duration_ms("30", CustomDurationUnit::Seconds)
            .expect("30 seconds is a duration a host must be able to type");
        assert_eq!(ms, 30_000);
        assert_eq!(
            format_duration_words(ms),
            "30 seconds",
            "the confirm button spells the resulting duration out with this, so a \
             wrong conversion would also mislabel the button"
        );
    }

    /// Minutes convert against the SAME constant the preset row is built from, so
    /// typing a preset's number cannot mean something different from clicking it.
    ///
    /// MUTATION PROOF: change `CustomDurationUnit::Minutes.ms()` from 60_000 and
    /// both assertions go red.
    #[test]
    fn a_typed_minute_count_means_the_same_as_the_matching_preset() {
        assert_eq!(
            custom_duration_ms("5", CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_PRESETS_MS[1])
        );
        assert_eq!(
            custom_duration_ms("15", CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_PRESETS_MS[3])
        );
    }

    /// The unit the control OPENS on has to be the one the presets are labelled
    /// in, or a host who types "5" alongside a row of "min" buttons gets five
    /// SECONDS -- a timer that buzzes at the whole room almost immediately.
    ///
    /// MUTATION PROOF: flip `MEETING_TIMER_CUSTOM_DEFAULT_UNIT` to `Seconds` and
    /// this goes red. (Asserting the constant against itself would pin nothing;
    /// this asserts what the default MEANS.)
    #[test]
    fn the_default_unit_matches_the_unit_the_presets_are_labelled_in() {
        assert_eq!(
            custom_duration_ms("5", MEETING_TIMER_CUSTOM_DEFAULT_UNIT),
            Some(MEETING_TIMER_PRESETS_MS[1]),
            "the presets read '5 min', so typing 5 into the field as it opens must \
             mean the same five minutes"
        );
    }

    /// Everything that is NOT a duration returns `None`, which is what disables
    /// the confirm button. A live-looking button that starts something the host
    /// did not type is the failure this prevents.
    ///
    /// `+5` is in the list on purpose. `u64::from_str` ACCEPTS a leading `+`, so
    /// a digit check applied only as a fallback AFTER a failed parse would take
    /// `+5` while refusing `+` followed by an over-length run of digits — an
    /// asymmetry with no reason a caller could infer. Checking the digits first
    /// refuses every signed entry alike, and this case pins that.
    ///
    /// MUTATION PROOF: drop the `amount == 0` guard and "0" returns `Some(0)`;
    /// drop the `trimmed.is_empty()` guard and "" passes the digit check
    /// vacuously (`"".bytes().all(..)` is true), fails to parse, and clamps to
    /// the 24h cap; drop the digit check and "1.5"/"abc"/"+5" clamp to the cap
    /// instead of rejecting.
    #[test]
    fn an_entry_that_is_not_a_duration_disables_the_confirm_button() {
        for junk in [
            "",
            "   ",
            "0",
            "000",
            "-5",
            "+5",
            "1.5",
            ".5",
            "1e3",
            "abc",
            "5m",
            "5 minutes",
            "+",
            "١٢",
        ] {
            assert_eq!(
                custom_duration_ms(junk, CustomDurationUnit::Seconds),
                None,
                "{junk:?} is not a whole count of units and must leave the confirm \
                 button disabled"
            );
            assert_eq!(custom_duration_ms(junk, CustomDurationUnit::Minutes), None);
        }
    }

    /// Surrounding whitespace is the host's, not an error -- a pasted value
    /// carries it routinely.
    ///
    /// MUTATION PROOF: drop the `.trim()` and " 30 " fails to parse, fails the
    /// all-digits check, and returns `None`.
    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            custom_duration_ms(" 30 ", CustomDurationUnit::Seconds),
            Some(30_000)
        );
    }

    /// An over-cap amount CLAMPS. The relay drops an over-cap START at ingress
    /// rather than clamping it, so sending one unclamped leaves the host looking
    /// at a local echo of a timer nobody else can see.
    ///
    /// MUTATION PROOF: remove the `clamp_duration_ms` call and the first case
    /// returns 120_000_000; replace `saturating_mul` with `*` and the
    /// all-digits-overflow case panics in a debug build.
    #[test]
    fn an_over_cap_entry_clamps_to_the_relay_ceiling() {
        assert_eq!(
            custom_duration_ms("2000", CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_MAX_DURATION_MS),
            "the relay DROPS an over-cap START instead of clamping it, so the UI \
             has to clamp before it sends"
        );
        assert_eq!(
            custom_duration_ms("999999999", CustomDurationUnit::Seconds),
            Some(MEETING_TIMER_MAX_DURATION_MS)
        );
        // Longer than `u64` can hold: still a request for more time than exists,
        // not junk, so it clamps rather than blanking the button.
        assert_eq!(
            custom_duration_ms("18446744073709551616", CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_MAX_DURATION_MS)
        );
    }

    /// The multiply must SATURATE. An amount that fits in `u64` can still
    /// overflow once scaled to milliseconds, and a wrapped product does not land
    /// somewhere obviously wrong — it lands back inside the cap, as a
    /// plausible-looking timer that starts and buzzes almost immediately.
    ///
    /// THE WITNESS MATTERS, and the obvious one does not work. Neither `u64::MAX`
    /// nor a digit run too long to parse distinguishes the two spellings:
    /// `u64::MAX.wrapping_mul(60_000)` is `2^64 - 60_000`, still astronomically
    /// over the cap, so both forms clamp and the test passes on the broken code.
    /// (That is the same trap `urgency_does_not_overflow_on_a_huge_remaining`
    /// documents one function up, and this test was written with the useless
    /// witness first — it stayed green against `wrapping_mul` until the value
    /// below replaced it.)
    ///
    /// `18_446_744_073_709_552` seconds is the value that bites: times 1_000 it
    /// is 384 ms past `2^64`, so the wrapping form returns `Some(384)` — a
    /// third-of-a-second timer — where saturating returns the 24h ceiling.
    ///
    /// MUTATION PROOF: swap `saturating_mul` for `wrapping_mul` and this goes red.
    #[test]
    fn scaling_to_milliseconds_saturates_rather_than_wrapping() {
        assert_eq!(
            custom_duration_ms("18446744073709552", CustomDurationUnit::Seconds),
            Some(MEETING_TIMER_MAX_DURATION_MS),
            "a wrapped product lands back INSIDE the cap (384 ms here), so the \
             host would ask for longer than the universe and get a timer that \
             expires before they let go of the button"
        );
    }

    /// The exact ceiling in BOTH units, plus the value one step below it. The
    /// second half is what stops a "clamp" that is really a constant: replace the
    /// clamp with `MEETING_TIMER_MAX_DURATION_MS` and the 1439-minute assertion
    /// goes red while every over-cap assertion stays green.
    #[test]
    fn the_exact_cap_is_accepted_and_the_step_below_it_is_untouched() {
        let max_minutes = MEETING_TIMER_MAX_DURATION_MS / 60_000;
        let max_seconds = MEETING_TIMER_MAX_DURATION_MS / 1_000;
        assert_eq!(
            custom_duration_ms(&max_minutes.to_string(), CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_MAX_DURATION_MS)
        );
        assert_eq!(
            custom_duration_ms(&max_seconds.to_string(), CustomDurationUnit::Seconds),
            Some(MEETING_TIMER_MAX_DURATION_MS)
        );
        assert_eq!(
            custom_duration_ms(&(max_minutes - 1).to_string(), CustomDurationUnit::Minutes),
            Some(MEETING_TIMER_MAX_DURATION_MS - 60_000),
            "a sub-cap value must pass through untouched -- clamping everything to \
             the ceiling would satisfy every over-cap test and still be wrong"
        );
    }

    /// The smallest timer the field can produce. There is no floor server-side
    /// (`packet_handler` bounds `duration_ms` only from ABOVE, plus the
    /// `ends_at_ms >= duration_ms` consistency check), so one second is a legal
    /// timer and must not be rejected as if it were junk.
    #[test]
    fn one_second_is_the_floor_and_it_is_accepted() {
        assert_eq!(
            custom_duration_ms("1", CustomDurationUnit::Seconds),
            Some(1_000)
        );
        let s = start_state(1_000, 1_700_000_000_000);
        assert!(
            s.ends_at_ms >= s.duration_ms,
            "the relay's internal-consistency check must hold for the shortest \
             timer the field can produce"
        );
    }

    /// The `<option value>` strings and the parser that reads them back must
    /// agree, since a mismatch would silently pin the unit at whatever it was.
    ///
    /// MUTATION PROOF: make `as_value` return the same string for both variants
    /// and the round trip maps Seconds to Minutes -> red.
    #[test]
    fn unit_option_values_round_trip() {
        for unit in [CustomDurationUnit::Seconds, CustomDurationUnit::Minutes] {
            assert_eq!(
                CustomDurationUnit::from_value(unit.as_value()),
                Some(unit),
                "{unit:?} must survive the trip through its <option value>"
            );
        }
        assert_ne!(
            CustomDurationUnit::Seconds.as_value(),
            CustomDurationUnit::Minutes.as_value(),
            "two units sharing an <option value> would make the control unable to \
             report which one was picked"
        );
    }

    /// An unrecognised value leaves the unit alone rather than resolving to a
    /// default, so a tampered DOM cannot start a timer of a length the host never
    /// picked.
    #[test]
    fn an_unknown_unit_value_is_refused_rather_than_defaulted() {
        assert_eq!(CustomDurationUnit::from_value("hours"), None);
        assert_eq!(CustomDurationUnit::from_value(""), None);
        assert_eq!(CustomDurationUnit::from_value("Minutes"), None);
    }

    /// The confirm button's accessible name and the hint are the ONLY places a
    /// host is told what the entry resolved to — including that it was clamped,
    /// which nothing else surfaces. Both are asserted through the production
    /// composers rather than re-formatted here.
    ///
    /// MUTATION PROOF: make either composer ignore its argument and return a
    /// fixed string, and the corresponding pair of assertions goes red.
    #[test]
    fn the_button_and_hint_spell_out_what_the_entry_resolved_to() {
        let thirty_s = custom_duration_ms("30", CustomDurationUnit::Seconds);
        assert_eq!(
            compose_custom_start_label(thirty_s),
            "Start a 30 seconds timer"
        );
        assert_eq!(
            compose_custom_hint(thirty_s),
            "Timer will run for 30 seconds."
        );

        // A CLAMPED entry: the host typed 2000 minutes and both surfaces say 24
        // hours, which is the only warning that the cap was applied.
        let clamped = custom_duration_ms("2000", CustomDurationUnit::Minutes);
        assert_eq!(
            compose_custom_start_label(clamped),
            "Start a 24 hours timer"
        );
        assert_eq!(compose_custom_hint(clamped), "Timer will run for 24 hours.");

        // An entry that does not parse: the button still has a name (it stays in
        // the tab order under `aria-disabled`), and the hint states the rule,
        // which is the only place the reason is available.
        assert_eq!(compose_custom_start_label(None), "Start a custom timer");
        assert_eq!(
            compose_custom_hint(None),
            "Enter a whole number, then pick minutes or seconds."
        );
    }

    /// A typed duration takes the SAME start path as a preset, so everything the
    /// wire contract requires of a preset holds for it: `start_state` clamps, and
    /// `ends_at_ms >= duration_ms` -- the two conditions the relay checks before
    /// it will broadcast a START.
    #[test]
    fn a_typed_duration_produces_a_start_the_relay_will_accept() {
        let now = 1_700_000_000_000;
        for (raw, unit) in [
            ("30", CustomDurationUnit::Seconds),
            ("90", CustomDurationUnit::Minutes),
            ("999999999", CustomDurationUnit::Minutes),
        ] {
            let ms = custom_duration_ms(raw, unit).expect("a valid entry");
            let s = start_state(ms, now);
            assert!(s.running);
            assert!(
                s.duration_ms <= MEETING_TIMER_MAX_DURATION_MS,
                "{raw} {unit:?} exceeds the cap the relay drops on"
            );
            assert!(s.ends_at_ms >= s.duration_ms);
            assert_eq!(
                s.duration_ms, ms,
                "the duration the button advertised must be the duration that \
                 starts -- `start_state` clamping again would mean the label and \
                 the timer could disagree"
            );
        }
    }

    #[test]
    fn urgency_modifiers_are_distinct() {
        let mods: Vec<&str> = [
            TimerUrgency::Normal,
            TimerUrgency::Warning,
            TimerUrgency::Critical,
            TimerUrgency::Expired,
        ]
        .iter()
        .map(|u| u.modifier())
        .collect();
        let mut sorted = mods.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            mods.len(),
            "each urgency needs its own CSS modifier or two states would style alike"
        );
    }
}
