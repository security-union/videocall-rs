// SPDX-License-Identifier: MIT OR Apache-2.0

//! The raised-hand roster: its ordering rules, its copy, and the persistent
//! banner that renders it (issue 2135).
//!
//! The issue's load-bearing clause is "peers see who has the hand raised **even
//! if the tile is not been displayed**". A tile badge alone cannot satisfy that:
//! the decode budget sheds tiles, the grid paginates, a screen share collapses
//! the grid to a strip, and a camera-off participant may have no tile at all. So
//! the banner — a room-wide list that is independent of tile visibility — is the
//! surface that actually answers the issue; the tile and roster badges are
//! locality affordances layered on top.
//!
//! Everything except the component itself is pure and driven by plain `#[test]`,
//! which is the only test gate that executes for this crate
//! (`#[wasm_bindgen_test]` compiles here but never runs in CI).

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::Cell;
use std::rc::Rc;
// (Issue 2329 briefly imported `RAISE_HAND_REANNOUNCE_COALESCE_MS` to size an
// anti-storm window against it. The window is gone — the gate now compares the
// sender's raise stamp against our own join instant — so the coalesce window is
// no longer an input to anything here, and neither is the import.)

use crate::components::icons::raised_hand::RaisedHandIcon;
use crate::context::RaisedHandsCtx;

/// How many names the banner spells out before collapsing the rest into
/// "and N others".
///
/// Three is the point at which a comma list stops being scannable at a glance
/// while a call is in progress. It also bounds the banner's width so it cannot
/// grow to cover the video in a large meeting — the failure mode a naive
/// "list everyone" banner hits at exactly the moment (many hands up) the feature
/// matters most.
pub const RAISED_HANDS_BANNER_MAX_NAMES: usize = 3;

/// Minimum spacing between screen-reader announcements of the raised-hand
/// roster, in milliseconds.
///
/// Deliberately LONGER than the reactions equivalent (2000 ms). A reaction is a
/// discrete event a screen-reader user wants to hear; a raised-hand roster is
/// AMBIENT STATE, and in a 20-person meeting where hands go up in a wave, an
/// announcement per change would talk over everything else the user is doing.
/// Each flush announces the CURRENT roster summary rather than the individual
/// changes, so one utterance covers an arbitrary number of coalesced updates.
pub const RAISED_HANDS_SR_THROTTLE_MS: u32 = 4000;

/// One participant's raised hand, as the room renders it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaisedHand {
    /// The relay-stamped envelope session id of the raiser — the ONLY
    /// authoritative attribution (unforgeable since #2124) and the deterministic
    /// tie-break for equal `raised_at_ms`.
    pub session_id: u64,
    /// The raiser's own wall clock at its false→true transition. ADVISORY
    /// DISPLAY ORDERING ONLY: forgeable, skewed across clients, and never an
    /// input to any authorization decision (see the proto doc).
    pub raised_at_ms: u64,
    /// Resolved, sanitized display name (control-stripped, <=64 chars). Rendered
    /// as escaped text — Dioxus escapes by default and the resolver strips
    /// control characters on top of that.
    pub name: String,
    /// Whether this is the LOCAL participant, which changes the banner's copy
    /// ("You raised your hand") and lets the roster row skip the ordinal chrome
    /// it already conveys as "(You)".
    pub is_self: bool,
}

/// Assign an absolute RAISED level to the entry matched by `is_match`, keeping
/// `list` sorted by `(raised_at_ms, session_id)`. Returns whether the list
/// actually CHANGED.
///
/// The return value is load-bearing, not a convenience: callers only take a
/// `Signal::write()` when it is `true`. An unconditional write marks the signal
/// dirty, which re-renders the banner and the roster and re-runs every mounted
/// tile's raised-hand `use_memo` — even when nothing moved. A re-announce
/// delivers the exact same `(raised_at_ms, name)` for an already-raised hand once
/// per joining peer, so redundant updates are the COMMON case here, not a
/// hypothetical one.
///
/// (The tiles themselves no longer RE-RENDER on an unrelated change — their memo
/// short-circuits on an unchanged `bool` — but the write still costs a wake-up
/// per tile, and the two surfaces that do subscribe directly re-render for
/// nothing.)
fn upsert_raised_hand(
    list: &mut Vec<RaisedHand>,
    hand: RaisedHand,
    is_match: impl Fn(&RaisedHand) -> bool,
) -> bool {
    if let Some(existing) = list.iter_mut().find(|h| is_match(h)) {
        if *existing == hand {
            return false;
        }
        // A name can arrive late (the display-name cache loses the race with a
        // re-announce) and a re-raise carries a NEW stamp, so update in place
        // and re-sort rather than assuming the position is still correct.
        *existing = hand;
        sort_raised_hands(list);
        return true;
    }
    list.push(hand);
    sort_raised_hands(list);
    true
}

/// Does `h` identify the REMOTE session `session_id`?
///
/// The single definition of the peer keyspace, shared by the mutators and by the
/// `would_*_change` predicates below so the two can never drift — a predicate
/// that disagreed with its mutator would either skip a real update or reinstate
/// the redundant write it exists to avoid.
///
/// Matching deliberately EXCLUDES the local participant's own entry, making the
/// self and peer keyspaces disjoint. Without that exclusion, the two could
/// collide in one (astronomically unlikely but real) case: the local entry falls
/// back to `u64::MAX` while our session id is not yet assigned, and a peer's
/// relay-stamped id is a random 64-bit value (`Uuid::new_v4() as u64` in
/// `session_logic`), so `u64::MAX` is a legal peer id. Keying the two paths
/// differently costs nothing and removes the case entirely rather than relying on
/// a 2^-64 argument.
fn is_peer_entry(h: &RaisedHand, session_id: u64) -> bool {
    !h.is_self && h.session_id == session_id
}

/// Assign a REMOTE session's absolute raised level. Returns whether the list
/// changed.
///
/// ASSIGN, never toggle: `raised` is a level on the wire, so a duplicated packet
/// must be a no-op. This function has no way to express "flip", by design.
pub fn set_raised_hand(list: &mut Vec<RaisedHand>, hand: RaisedHand) -> bool {
    debug_assert!(
        !hand.is_self,
        "set_raised_hand is the REMOTE path; use set_self_raised_hand for the local participant",
    );
    let session_id = hand.session_id;
    upsert_raised_hand(list, hand, |h| is_peer_entry(h, session_id))
}

/// Would [`set_raised_hand`] change anything? Answers WITHOUT needing an owned
/// copy of the list.
///
/// Exists purely to keep the inbound-packet path allocation-free in its common
/// case. The caller holds a `Signal<Vec<RaisedHand>>`, and mutating it requires
/// cloning the whole `Vec` (every `RaisedHand`, every `String`) out of the signal
/// first. A REDUNDANT re-announce is the COMMON inbound event — every peer that
/// joins triggers one per raised hand — so paying that clone unconditionally
/// meant allocating a roster copy per packet only to throw it away. This lets the
/// caller peek, decide, and clone only when the clone is going to be used.
pub fn would_set_raised_hand_change(list: &[RaisedHand], hand: &RaisedHand) -> bool {
    match list.iter().find(|h| is_peer_entry(h, hand.session_id)) {
        Some(existing) => existing != hand,
        None => true,
    }
}

/// Drop a REMOTE session's hand. Returns whether the list changed.
///
/// Used for both an explicit LOWER and the departure cleanup — a departing
/// participant broadcasts nothing, and the relay holds no hand registry, so
/// without this a hand stays up forever after its owner leaves.
///
/// Never touches the local participant's entry (see [`is_peer_entry`]).
pub fn clear_raised_hand(list: &mut Vec<RaisedHand>, session_id: u64) -> bool {
    let before = list.len();
    list.retain(|h| !is_peer_entry(h, session_id));
    list.len() != before
}

/// Would [`clear_raised_hand`] change anything? See
/// [`would_set_raised_hand_change`] for why the caller needs to know before it
/// clones.
pub fn would_clear_raised_hand_change(list: &[RaisedHand], session_id: u64) -> bool {
    list.iter().any(|h| is_peer_entry(h, session_id))
}

/// Assign the LOCAL participant's absolute raised level. Returns whether the
/// list changed.
///
/// Keyed on `is_self`, NOT on the session id, and that is the whole point: the
/// local session id is resolved fresh at each call from
/// `VideoCallClient::get_own_session_id()`, which can legitimately return a
/// DIFFERENT value than it did a moment ago (it is `None` before
/// `SESSION_ASSIGNED`, and changes outright on a re-election). Keying on it would
/// let a raise and its matching lower disagree, orphaning the local entry in the
/// banner with no way to clear it.
pub fn set_self_raised_hand(list: &mut Vec<RaisedHand>, hand: RaisedHand) -> bool {
    debug_assert!(
        hand.is_self,
        "set_self_raised_hand is the LOCAL path; use set_raised_hand for remote sessions",
    );
    upsert_raised_hand(list, hand, |h| h.is_self)
}

/// Drop the LOCAL participant's hand. Returns whether the list changed.
///
/// Keyed on `is_self` for the reason spelled out on [`set_self_raised_hand`]: a
/// lower must succeed even when the session id has changed since the raise.
pub fn clear_self_raised_hand(list: &mut Vec<RaisedHand>) -> bool {
    let before = list.len();
    list.retain(|h| !h.is_self);
    list.len() != before
}

/// Re-stamp the local entry with the CURRENT session id, re-sorting if the
/// tie-break moved. Returns whether the list changed.
///
/// Called on every `on_connected`. A re-election mints a new session id, and the
/// local entry would otherwise keep the old one — which is what the roster row
/// looks itself up by (`RaisedHandsCtx::queue_slot(current_session_id)`), so the
/// local user's own roster badge would silently vanish while their hand was still
/// up and still visible to everyone else. `raised_at_ms` is deliberately
/// untouched: it is the ordering key, and a re-election must not move the local
/// user in the queue.
pub fn resync_self_session_id(list: &mut [RaisedHand], session_id: u64) -> bool {
    let Some(existing) = list.iter_mut().find(|h| h.is_self) else {
        return false;
    };
    if existing.session_id == session_id {
        return false;
    }
    existing.session_id = session_id;
    sort_raised_hands(list);
    true
}

/// The room-wide ordering: ascending `raised_at_ms`, tie-broken on `session_id`.
///
/// The tie-break is not cosmetic. `raised_at_ms` is each sender's OWN wall clock,
/// so two participants can legitimately publish the same millisecond, and a
/// malicious one can publish any value at all. Without a deterministic secondary
/// key, two participants could render the same two hands in opposite orders and
/// disagree about who is next — so the ordering must never depend on arrival
/// order or hash iteration.
fn sort_raised_hands(list: &mut [RaisedHand]) {
    list.sort_by(|a, b| {
        a.raised_at_ms
            .cmp(&b.raised_at_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
}

/// How the LOCAL participant is spelled in any name list.
///
/// Every list-shaped surface in this module renders on the local user's OWN
/// screen, so spelling their resolved display name there makes their own banner
/// read "Antonio and Alice raised their hands" — their own name, in the third
/// person, about themselves. One constant so the banner sentence, the compact
/// form and the screen-reader summary cannot drift apart on this.
const SELF_NAME: &str = "You";

/// Join up to [`RAISED_HANDS_BANNER_MAX_NAMES`] names into a readable phrase,
/// collapsing any remainder into "and N others".
///
/// No serial comma, in either the plain or the overflow form. The two branches
/// used to disagree ("Alice, Bob and Carol" against "Alice, Bob, Carol, and 2
/// others"), so one of them had to move; the product voice elsewhere omits it.
fn join_names(list: &[RaisedHand]) -> String {
    let shown: Vec<&str> = list
        .iter()
        .take(RAISED_HANDS_BANNER_MAX_NAMES)
        .map(|h| {
            if h.is_self {
                SELF_NAME
            } else {
                h.name.as_str()
            }
        })
        .collect();
    let hidden = list.len().saturating_sub(shown.len());

    let tail = if hidden == 1 {
        Some("1 other".to_string())
    } else if hidden > 1 {
        Some(format!("{hidden} others"))
    } else {
        None
    };

    match (shown.as_slice(), tail) {
        ([], None) => String::new(),
        ([], Some(t)) => t,
        ([only], None) => only.to_string(),
        (many, None) => {
            let (last, head) = many.split_last().expect("non-empty");
            format!("{} and {}", head.join(", "), last)
        }
        (many, Some(t)) => format!("{} and {}", many.join(", "), t),
    }
}

/// Is the local participant among the names [`join_names`] actually SPELLS OUT?
///
/// Not "is our hand up anywhere in the roster" — the distinction is the whole
/// reason this is its own function. The banner's possessive has to agree with
/// the SUBJECT of its sentence, and the subject is the joined phrase, not the
/// roster. With four hands up and ours fourth, the phrase is "Alice, Bob, Carol
/// and 1 other" — a wholly third-person subject — so "your hands" would be
/// wrong there even though our own hand is up.
fn self_is_named(list: &[RaisedHand]) -> bool {
    list.iter()
        .take(RAISED_HANDS_BANNER_MAX_NAMES)
        .any(|h| h.is_self)
}

/// The banner's visible sentence, or `None` when no hand is up (the banner then
/// renders nothing at all — no empty chrome parked over the video).
///
/// `list` must already be in raise order; the phrasing communicates the queue by
/// listing names in that order, so passing an unsorted list would silently
/// mis-state who is next.
pub fn compose_raised_hands_banner(list: &[RaisedHand]) -> Option<String> {
    match list.len() {
        0 => None,
        // Singular self gets its own phrasing: "You raised their hand" is wrong,
        // and it is the FIRST thing a user sees after pressing the control, so
        // it is the copy most likely to be read closely.
        1 if list[0].is_self => Some("You raised your hand".to_string()),
        1 => Some(format!("{} raised their hand", list[0].name)),
        // "You and Alice raised their hands" is ungrammatical, and it is what
        // the third-person plural produced as soon as `join_names` learned to
        // say "You". English resolves a mixed second/third-person subject to the
        // SECOND person ("you and Alice ... your hands"), so the possessive keys
        // on whether we are one of the names actually spelled out — see
        // [`self_is_named`] for why "anywhere in the roster" is the wrong test.
        _ => {
            let possessive = if self_is_named(list) { "your" } else { "their" };
            Some(format!("{} raised {possessive} hands", join_names(list)))
        }
    }
}

/// "N hands raised" — the count phrasing used by the screen-reader summary.
///
/// Only ever reached with `n >= 1`: its sole caller,
/// [`compose_raised_hands_announcement`], returns early on an empty roster, and
/// a drained roster is spoken as "All hands lowered" by
/// [`raised_hands_live_text`] instead. The `n == 0` arm is therefore total-ness,
/// not behaviour, and is deliberately NOT pinned by a test — an assertion about
/// a string nothing can produce pins nothing.
pub fn compose_raised_hands_count(n: usize) -> String {
    if n == 1 {
        "1 hand raised".to_string()
    } else {
        format!("{n} hands raised")
    }
}

/// The compact banner text shown on narrow viewports: the FIRST raiser's name,
/// plus "+N" for everyone behind them.
///
/// The issue asks who has a hand raised. The previous mobile treatment hid the
/// name list entirely and showed only "N hands raised", which answers *how many*
/// and never *who* — on the class of device where the roster drawer and the tile
/// badges are hardest to reach. Naming the head of the queue answers the
/// question for the person it is actually about (the one who is next), and "+N"
/// preserves the count that phrasing would otherwise lose.
///
/// `None` for an empty roster, mirroring [`compose_raised_hands_banner`]: with no
/// hand up the banner does not render at all.
pub fn compose_raised_hands_compact(list: &[RaisedHand]) -> Option<String> {
    let first = list.first()?;
    // Second person for our own hand, for the same reason the full sentence
    // does: "Antonio +2" about yourself reads as someone else.
    let name = if first.is_self {
        SELF_NAME
    } else {
        &first.name
    };
    match list.len() - 1 {
        0 => Some(name.to_string()),
        behind => Some(format!("{name} +{behind}")),
    }
}

/// The throttled screen-reader utterance for the CURRENT roster.
///
/// Announces STATE, not events, which is what keeps a 20-person hand wave from
/// becoming 20 utterances: however many changes coalesce into one throttle
/// window, the result is a single summary. Returns `None` when the roster is
/// empty — an emptied roster is announced by the caller as an explicit
/// "all hands lowered" rather than by clearing the live region (clearing a live
/// region announces nothing, so the user would never learn the queue drained).
pub fn compose_raised_hands_announcement(list: &[RaisedHand]) -> Option<String> {
    if list.is_empty() {
        return None;
    }
    Some(format!(
        "{}. {}.",
        compose_raised_hands_count(list.len()),
        join_names(list)
    ))
}

/// What the live region should say for a roster transition, including the
/// drained case. Pure so the "all hands lowered" edge is covered by a host test.
pub fn raised_hands_live_text(list: &[RaisedHand]) -> String {
    compose_raised_hands_announcement(list).unwrap_or_else(|| "All hands lowered".to_string())
}

/// Should the throttled flush actually WRITE `next` into the live region?
///
/// `last_announced` is the region's current text (empty = nothing has ever been
/// announced this session) and `roster_empty` is whether any hand is up RIGHT NOW.
///
/// Two independent suppressions, and the first is the non-obvious one:
///
/// 1. **Nothing announced yet AND nothing up now.** Two distinct situations
///    reach this, both of which must stay silent. The banner mounts with an
///    empty roster, so its first throttle window would otherwise announce "All
///    hands lowered" on entering EVERY meeting, about hands nobody raised. And a
///    raise whose matching lower lands inside the SAME throttle window nets to
///    zero — announcing the lowering of a raise the user never heard is worse
///    than saying nothing.
///
///    Note this check has to live HERE, at the write, not only at the point
///    where the timer is armed: a lower that arrives while a timer is already
///    armed cannot un-arm it, so an arm-time guard alone would still let the
///    stale timer fire and speak.
///
/// 2. **No change.** Writing an identical string neither re-announces (the DOM
///    text is unchanged, so assistive tech has nothing to react to) nor is free —
///    it dirties the component for nothing.
pub fn should_announce_roster(last_announced: &str, roster_empty: bool, next: &str) -> bool {
    if last_announced.is_empty() && roster_empty {
        return false;
    }
    last_announced != next
}

/// The accessible name for the ROSTER row's raised-hand badge.
///
/// Deliberately does NOT name the participant. The badge is rendered inside the
/// roster row whose first content is that same name, so including it made AT
/// read "Alice … Alice raised their hand". The badge's job is to add the state
/// the row does not already carry.
///
/// The queue slot is spelled "position 2 of 5", not "2 in the queue": the latter
/// reads as a COUNT ("there are 2 people in the queue"), which is the opposite of
/// what it means, and an ordinal without its total tells a screen-reader user
/// their rank without telling them the size of the thing they are ranked in.
///
/// Only the roster passes a slot. The tile badge uses the icon's default bare
/// "Hand raised" — see `hand_raised` on `canvas_generator::generate_for_peer` for
/// why an ordinal must not be resolved per tile.
pub fn raised_hand_badge_label(slot: Option<(usize, usize)>) -> String {
    match slot {
        Some((position, total)) => format!("Hand raised, position {position} of {total}"),
        None => "Hand raised".to_string(),
    }
}

/// The accessible name for the SELF tile's raised-hand badge.
///
/// Deliberately NOT `raised_hand_badge_label(None)`. That returns the bare "Hand
/// raised" the peer tiles use, which is correct on a tile labelled with somebody
/// else's name and needlessly vague on our own: this badge renders inside the
/// local participant's own tile chrome, where the second person is both
/// available and more precise. It lives here rather than inline in
/// `attendants.rs` so this module owns every string the feature speaks — the
/// self tile was the one surface whose copy was stranded elsewhere.
pub const SELF_RAISED_HAND_BADGE_LABEL: &str = "Your hand is raised";

// ──────────────────────────────────────────────────────────────────────────
// The raise/lower chime policy (issue 2329).
//
// The DECISION lives here — pure, and driven by plain `#[test]`, which is the
// gate `pr-check-rust-hcl.yaml` actually executes for this crate
// (`cargo test -p videocall-ui --lib`). The EFFECT (`play_tone_pair` and the two
// wrappers around it) lives in `attendants.rs` next to the join/leave chimes it
// is modelled on, because that is where the Web Audio primitive already is.
// ──────────────────────────────────────────────────────────────────────────

/// The audible consequence of one raised-hand LEVEL transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandSound {
    /// A hand went up. Rendered as an ASCENDING pair, matching the join chime's
    /// "something arrived" direction.
    Raised,
    /// A hand came down because its owner lowered it. Rendered as a DESCENDING
    /// pair.
    ///
    /// NOT emitted for the departure cleanup: a participant who disconnects with
    /// a hand up has their entry retracted by [`clear_raised_hand`], but they did
    /// not lower it, and that departure already has its own leave chime. Chiming
    /// there would report one event twice and mis-describe it. The guarantee is
    /// STRUCTURAL rather than a flag — `on_peer_left`'s `ClearRaisedHand` arm
    /// does not call the sound helper at all — so there is no condition here to
    /// get wrong.
    Lowered,
}

/// How much sender clock skew the "was this raised after I joined?" test
/// tolerates, in milliseconds.
///
/// THE ONLY error term left in the anti-storm gate, and it is worth being
/// precise about what it costs in each direction because both are real:
///
/// * A sender whose clock runs FAST by more than this can have a hand that was
///   already up read as raised after we joined, and chime once on our arrival.
///   One chime, and the rate gate bounds a roomful of them to one.
/// * A sender whose clock runs SLOW by more than this loses the chime for a
///   raise made within that skew OF OUR OWN CONNECT — and only then. Every
///   later raise is correct.
///
/// That asymmetry is the whole reason this shape was chosen over a "was the
/// raise recent?" band. Under a band, a peer whose clock is off by more than the
/// band is silent FOREVER — an undiagnosable, permanently silent feature for
/// that person. Here the penalty is confined to a window around our own connect
/// and then disappears, however badly skewed the peer is.
///
/// ## The polarity is deliberate — do not "fix" the asymmetry
///
/// The margin is ADDED to our own join instant, so the test suppresses iff
/// `raised_at_ms <= connected_at_ms + MARGIN`. That biases toward SILENCE at the
/// boundary in both readings: a genuine raise in the first two seconds after we
/// join may be missed, and a hand that was already up will not chime. It is not
/// symmetric and it is not meant to be — a missed chime is recoverable (the
/// banner still shows the hand, and the next transition speaks), while a storm is
/// the bug this whole gate exists to prevent. Moving the margin to the other side
/// of the comparison, or splitting it into two half-margins, would trade a
/// recoverable failure for an unrecoverable one.
///
/// 2000 ms covers ordinary unsynced-laptop drift. It is deliberately NOT larger:
/// every millisecond here is also a millisecond after our own connect during
/// which a genuine raise is silent, so the two costs trade directly against each
/// other and there is no free width.
pub const HAND_SOUND_CLOCK_SKEW_MARGIN_MS: f64 = 2_000.0;

/// Minimum spacing between two hand chimes, in milliseconds.
///
/// LEADING EDGE: the first hand of a wave chimes immediately (latency is the
/// whole value of an audio cue) and everything inside the window is DROPPED, not
/// queued. Queuing would keep the room clicking after the wave is over, which is
/// the mush this exists to prevent.
///
/// One gate shared by BOTH directions, deliberately. Two independent gates could
/// still overlap with each other — a raise and a lower 80 ms apart is exactly the
/// smear this is for — so the budget has to cover hand audio as a whole. 600 ms
/// leaves a 420 ms silence floor around the 180 ms tone pair, so two chimes can
/// never sound together, while raises a second apart still both speak.
///
/// It also bounds the Web Audio cost: `play_tone_pair` builds a NEW
/// `AudioContext` per chime and browsers cap how many a tab may hold, so a
/// Q&A hand-wave without this gate is not merely mush — later contexts start
/// failing to construct. At 600 ms against the ~280 ms lifetime of a hand
/// chime's context, at most one HAND context is ever live.
///
/// Scoped to HAND chimes on purpose, because that is all this gate can promise.
/// `play_user_joined` / `play_user_left` call `play_tone_pair` DIRECTLY — they
/// never pass through `maybe_play_hand_sound` — so a join or leave chime
/// (~340 ms context) can be live alongside a hand chime. Two concurrent contexts
/// sit comfortably inside any browser cap, so this is a scoping note rather than
/// a hazard; it is spelled out because "at most one is ever live" is exactly the
/// kind of claim someone would later lean on when deciding whether adding a
/// THIRD cue is safe. It would not cover them.
///
/// Those two have no rate limiter of their own — the leave path's
/// `mark_pending_leave_sound` / `take_pending_leave_sound` pair is a per-user
/// reconnect-flicker debounce, not a rate gate, and N distinct leavers still
/// yield N chimes. That is a pre-existing gap and not this gate's to close.
pub const HAND_SOUND_MIN_INTERVAL_MS: f64 = 600.0;

/// The hand-chime channel's state: when we joined, and when the channel last
/// spoke.
///
/// Held by the component in a `Cell` rather than a `Signal` because NOTHING
/// renders from it. A signal write here would dirty `AttendantsComponent` — the
/// #1296 / #2103 blast-radius hazard the roster's own comments describe — on
/// every chime, for no rendered change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandSoundChannel {
    /// Wall clock (`Date::now()`) at our most recent `on_connected`, or `None`
    /// before the first one.
    ///
    /// THE REFERENCE INSTANT the anti-storm gate compares against — not the
    /// start of a window. `None` means the channel is MUTE: with no idea when we
    /// joined, nothing can be shown to have happened after it.
    ///
    /// Re-stamped on EVERY connect, which is what makes a reconnect free: the
    /// member-list replay tears every raised hand down
    /// (`OnPeerLeftAction::ClearRaisedHand` runs before the reconnect
    /// early-return) and every peer re-announces, but each of those carries a
    /// `raised_at_ms` from before the new stamp and is therefore silent, while a
    /// genuine post-reconnect raise still speaks. One rule, no special case.
    pub connected_at_ms: Option<f64>,
    /// Wall clock at the last chime this channel actually played.
    /// `NEG_INFINITY` until the first one, so the first chime is never
    /// rate-gated.
    pub last_played_ms: f64,
}

impl Default for HandSoundChannel {
    fn default() -> Self {
        Self {
            connected_at_ms: None,
            last_played_ms: f64::NEG_INFINITY,
        }
    }
}

/// THE ANTI-STORM GATE: did this hand go up AFTER we joined?
///
/// `raised_at_ms` is the sender's own clock at its false→true edge, "set once at
/// the false->true transition and preserved verbatim across every re-announce"
/// (`raise_hand_packet.proto`). That is exactly — and uniquely — the fact we
/// need, because there is no hand registry anywhere: a joiner learns the room's
/// raised hands only because every peer holding one RE-ANNOUNCES it, so those
/// packets are, to us, brand-new `false → true` entries arriving in a burst,
/// packet-for-packet indistinguishable from people raising at once. That is the
/// shape of issue 2276 (14 stacked toasts on entering a populated meeting).
///
/// ## Why a comparison and not a window
///
/// Two window designs were built, measured end to end, and failed 5/5 — one
/// anchored on our own `on_connected`, one on each peer's relay introduction.
/// Both die for the same reason: the delay they must outlast is not bounded.
/// `on_connected` fires when the TRANSPORT comes up, and a client can then sit
/// in a waiting room until a host presses Admit, which has no upper bound. The
/// instrumented run showed introductions landing 1.9 s BEFORE `on_connected` and
/// the re-announces landing 5.5 s after the introductions, with both windows
/// already expired by ~620 ms and ~2.5 s respectively.
///
/// A "was the raise RECENT?" band fails too, and it is worth recording why so
/// nobody re-derives it: someone can raise a hand one second before we join, so
/// the age of an already-up hand has no lower bound and overlaps the age of a
/// live raise (coalesce + RTT). Overlapping populations cannot be separated by a
/// threshold.
///
/// Comparing the two timestamps asks the question we actually mean, and every
/// timing term — dwell, admit latency, RTT, the coalesce window, the length of
/// the join dance — drops out entirely rather than being estimated. The only
/// error term left is clock skew, bounded and explicit in
/// [`HAND_SOUND_CLOCK_SKEW_MARGIN_MS`].
///
/// ## On using `raised_at_ms` at all
///
/// The field is documented as advisory — forgeable and skewed, and never an
/// input to any AUTHORIZATION decision (see [`RaisedHand::raised_at_ms`]). This
/// is a second advisory use, recorded there too so the fence does not look
/// absolute. A chime is not an authorization decision: the worst a forger
/// achieves is deciding whether their OWN hand chimes on other people's
/// speakers, which they can already do by raising and lowering it.
///
/// The strictly more robust alternative is a wire bit distinguishing an original
/// from a re-announce — the sender knows which it is sending
/// (`RaiseHandTrigger`), and it would carry no clock dependency at all. It was
/// not taken here because it needs a proto change plus `videocall-client`, and
/// the safe polarity (`is_original`, so old clients defaulting to `false` read
/// as re-announce and stay silent) means it can be adopted later without
/// breaking mixed-version rooms.
/// ## Known residual: a rejoin can chime twice
///
/// A peer who raised AFTER we joined, then dropped and rejoined with the hand
/// still up, re-announces it. Their departure cleared our roster entry
/// (`OnPeerLeftAction::ClearRaisedHand`), so the re-announce reads as a fresh
/// `false → true`, and the stamp still postdates our join — so it chimes a
/// second time. The rate gate bounds it to one chime.
///
/// Left as-is deliberately. An earlier design carried a per-peer map that
/// happened to suppress this, and it cost ~150 lines and its own wedge hazards
/// to catch a case that is arguably CORRECT anyway: that hand did go up after we
/// arrived, and from our roster's point of view it has just gone up again.
fn raise_happened_after_we_joined(raised_at_ms: f64, connected_at_ms: Option<f64>) -> bool {
    let Some(connected_at_ms) = connected_at_ms else {
        return false;
    };
    raised_at_ms > connected_at_ms + HAND_SOUND_CLOCK_SKEW_MARGIN_MS
}

/// Which chime a LEVEL transition warrants, before any gating.
///
/// Keyed on the LEVEL, never on "did the roster change" — and that distinction is
/// the whole reason this takes two booleans rather than one `changed` flag. The
/// roster changes for things that are not transitions: a late-arriving display
/// name updates an already-raised entry in place (see [`upsert_raised_hand`] and
/// `a_late_display_name_updates_in_place`), and a re-raise carries a new stamp.
/// Both are `true → true` and must be SILENT. Gating on `changed` would chime for
/// a name resolving.
fn hand_sound_for_transition(was_raised: bool, now_raised: bool) -> Option<HandSound> {
    match (was_raised, now_raised) {
        (false, true) => Some(HandSound::Raised),
        (true, false) => Some(HandSound::Lowered),
        _ => None,
    }
}

/// May the hand-chime channel speak at `now_ms`, ignoring the anti-storm gate?
///
/// The preference, and the shared rate limiter. Split from the storm gate
/// because these two apply to BOTH directions while the storm gate applies only
/// to raises.
fn hand_sound_channel_open(channel: HandSoundChannel, enabled: bool, now_ms: f64) -> bool {
    if !enabled {
        return false;
    }
    let since_last = now_ms - channel.last_played_ms;
    // A backwards wall-clock step (NTP correction, laptop resume) makes
    // `since_last` negative, which a bare `>=` reads as "too soon" and would mute
    // the channel for the whole size of the step. The half-open range excludes
    // the negative side, so a stale watermark opens the gate and the chime that
    // follows re-seeds it forward.
    !(0.0..HAND_SOUND_MIN_INTERVAL_MS).contains(&since_last)
}

/// THE decision, and the only one any call site makes.
///
/// `raised_at_ms` is the sender's stamp from the packet, and is consulted ONLY
/// for a raise. A lower is always live: `raised_at_ms` is documented meaningless
/// (`0`) when `raised == false`, and — more to the point — a peer whose hand is
/// DOWN never re-announces at all, so there is no such thing as a replayed
/// lower. Feeding a lower through the storm gate would silence every genuine
/// lower in the room.
///
/// `is_self` skips the storm gate because a local press cannot be replay: it is
/// a user action that happened just now, by construction. The rate gate still
/// applies to it — a press should not smear over a remote chime from 100 ms ago,
/// and it already has three simultaneous visual confirmations (the control's
/// `aria-pressed`/`data-raised` state, the self-tile badge, and the banner).
pub fn hand_sound_to_play(
    channel: HandSoundChannel,
    raised_at_ms: f64,
    enabled: bool,
    is_self: bool,
    now_ms: f64,
    was_raised: bool,
    now_raised: bool,
) -> Option<HandSound> {
    let sound = hand_sound_for_transition(was_raised, now_raised)?;
    if !hand_sound_channel_open(channel, enabled, now_ms) {
        return None;
    }
    if sound == HandSound::Raised
        && !is_self
        && !raise_happened_after_we_joined(raised_at_ms, channel.connected_at_ms)
    {
        return None;
    }
    Some(sound)
}
// ──────────────────────────────────────────────────────────────────────────
// Dioxus component (thin driver over the pure helpers above).
// ──────────────────────────────────────────────────────────────────────────

/// The persistent raised-hands banner.
///
/// Reads [`RaisedHandsCtx`] HERE rather than in `AttendantsComponent` so a hand
/// going up re-renders this ~10-node child instead of the whole attendants RSX
/// (the #1296 / #2103 blast-radius hazard). Renders nothing when no hand is up.
///
/// This narrows the BANNER's own blast radius only; it says nothing about the
/// tile badges, which reach the roster by a different route entirely
/// (`PeerTile`'s `use_memo` over [`RaisedHandsCtx::is_raised`]). An earlier
/// version of this doc claimed the self-read also spared "every keyed
/// `PeerTile`" — it never did, because at that time each tile held its own
/// subscription to the same signal.
///
/// ## Accessibility
///
/// The visible banner is NOT a live region and carries NO live-region role. It
/// used to be `role="status"` + `aria-live="off"`, which is self-cancelling:
/// `role="status"` IS a live-region role, so the pair announced an element that
/// claims to be a status region and then never speaks. A plain element is the
/// honest encoding — a room where hands go up and down re-sorts this constantly,
/// and an implicitly-polite region would re-read the whole list on every change.
///
/// The single announcement channel is the throttled, visually-hidden region —
/// a SEPARATE, unconditionally-mounted component, [`RaisedHandsLiveRegion`].
/// Keeping it out of this component is what lets THIS one render nothing at all
/// while no hand is up (see that component's docs for why the SR node must never
/// be torn down, and why it is no longer a second root here).
#[component]
pub fn RaisedHandsBanner() -> Element {
    let hands = use_context::<RaisedHandsCtx>();
    let list = (hands.0)();

    let banner_text = compose_raised_hands_banner(&list);
    let compact = compose_raised_hands_compact(&list).unwrap_or_default();
    let count = list.len();

    // NOTHING is rendered while no hand is up — not even a wrapper. This
    // component contributes ZERO element nodes to `#grid-container` in the
    // common case, which is what keeps that container's child ordering (and the
    // positional selectors that read it) stable. See `RaisedHandsLiveRegion`.
    rsx! {
        if let Some(text) = banner_text {
            div {
                class: "raised-hands-banner",
                "data-testid": "raised-hands-banner",
                "data-hand-count": "{count}",

                span { class: "raised-hands-banner-icon", aria_hidden: "true",
                    RaisedHandIcon { decorative: true }
                }
                // Full sentence on wider viewports; the compact "name +N" form
                // replaces it ≤639px (CSS-driven, mirroring the decode-budget
                // banner). Only one of the two is visible at a time, so both are
                // readable text.
                span { class: "raised-hands-banner-text", "{text}" }
                span { class: "raised-hands-banner-compact", "{compact}" }
            }
        }
    }
}

/// The raised-hand roster's single screen-reader channel: visually hidden,
/// polite, atomic, and updated at most once per [`RAISED_HANDS_SR_THROTTLE_MS`]
/// with a summary of the CURRENT roster. `role="status"` + `aria-live="polite"`
/// are correct and reinforcing here — unlike on the visible banner, this element
/// really is the status region.
///
/// ## Why this is its own component
///
/// The SR node must NEVER be torn down and re-inserted: several AT/browser pairs
/// announce a live region's existing contents on insertion, so a region that is
/// destroyed and recreated on every empty↔non-empty transition comes back
/// already holding the previous text — the last hand lowering could then speak
/// "1 hand raised. Alice." at the very moment Alice lowered.
///
/// This used to be the unconditional SECOND ROOT of [`RaisedHandsBanner`]'s
/// template. That achieved the survival guarantee (Dioxus diffs the sibling `if`
/// in place against a placeholder) but made the banner component contribute one
/// PERMANENT element node to whatever container it was mounted in — a node that
/// sat at the FRONT of `#grid-container`'s children and shifted every positional
/// `nth-child()` reference to the split screen-share panes by one.
///
/// An always-mounted component of its own gives the same never-torn-down
/// guarantee more directly, and lets the caller put the SR node where it
/// perturbs nothing while the banner stays where its CSS needs it (an earlier
/// sibling of `.decode-budget-banner`, for the `~` stacking rule).
#[component]
pub fn RaisedHandsLiveRegion() -> Element {
    let hands = use_context::<RaisedHandsCtx>();

    // Throttled SR channel. `flush_scheduled` opens the window; the stored
    // Timeout is held (NOT `.forget()`-ed) so dropping this component cancels
    // it — a forgotten timer that writes a signal panics on a dropped scope in
    // dioxus-signals 0.7 (`set()` == `try_write().unwrap()`).
    let mut announcement = use_signal(String::new);
    let flush_scheduled = use_hook(|| Rc::new(Cell::new(false)));
    let mut sr_timer: Signal<Option<Timeout>> = use_signal(|| None);

    // Re-runs whenever the roster changes (the effect subscribes by reading the
    // context signal). It writes only `announcement` / `sr_timer`, neither of
    // which it reads, so there is no write-triggered re-run loop.
    {
        let flush_scheduled = flush_scheduled.clone();
        use_effect(move || {
            // Subscribe to the roster (a reactive read — this is what makes the
            // effect re-run on a change). The value is re-read INSIDE the timer
            // so the utterance describes the state at FLUSH time, not at schedule
            // time — that is what makes one utterance cover a whole wave.
            let roster = (hands.0)();
            // `peek`, NOT a reactive read: the timer below WRITES `announcement`,
            // so subscribing this effect to it would make every utterance re-run
            // the effect and re-arm the throttle forever.
            let last_announced = announcement.peek().clone();
            // Arm-time short-circuit: don't even start a throttle window for a
            // state that `should_announce_roster` would refuse to speak. This is
            // ONLY an optimisation (it saves a pointless 4s timer on entering
            // every meeting) — the load-bearing check is the identical one at
            // the WRITE below, because a change that arrives while a timer is
            // already armed cannot un-arm it.
            if !should_announce_roster(
                &last_announced,
                roster.is_empty(),
                &raised_hands_live_text(&roster),
            ) {
                return;
            }
            if flush_scheduled.get() {
                return;
            }
            flush_scheduled.set(true);
            let flush_scheduled = flush_scheduled.clone();
            sr_timer.set(Some(Timeout::new(RAISED_HANDS_SR_THROTTLE_MS, move || {
                flush_scheduled.set(false);
                // try_peek / try_write: this component can unmount between
                // arming and firing (hang up right after a hand goes up), and a
                // plain read()/set() panics on a dropped scope.
                let Ok(current) = hands.0.try_peek() else {
                    return;
                };
                let text = raised_hands_live_text(&current);
                let roster_empty = current.is_empty();
                drop(current);
                if let Ok(mut a) = announcement.try_write() {
                    // THE load-bearing guard (see `should_announce_roster`).
                    // Re-evaluated here against the roster as it stands NOW,
                    // because a lower that landed after this timer was armed
                    // could not cancel it — an arm-time check alone would let a
                    // raise-and-lower inside one window still speak "All hands
                    // lowered" about a raise the user never heard.
                    if should_announce_roster(&a, roster_empty, &text) {
                        *a = text;
                    }
                }
            })));
        });
    }

    rsx! {
        div {
            class: "visually-hidden",
            "data-testid": "raised-hands-live-region",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "{announcement}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand(session_id: u64, raised_at_ms: u64, name: &str) -> RaisedHand {
        RaisedHand {
            session_id,
            raised_at_ms,
            name: name.to_string(),
            is_self: false,
        }
    }

    // ===================================================================
    // Ordering + the session-id tie-break.
    // ===================================================================

    /// Hands render in raise order regardless of ARRIVAL order — the property
    /// that makes the banner a usable queue for a late joiner, who receives
    /// re-announces in whatever order the peers happen to send them.
    ///
    /// ADVERSARIAL (mutation): delete the `sort_raised_hands(list)` call after
    /// the `push` → the list keeps arrival order → red.
    #[test]
    fn hands_are_ordered_by_raise_time_not_arrival() {
        let mut list = Vec::new();
        // Arrive newest-first, the way a re-announce burst can.
        set_raised_hand(&mut list, hand(30, 3_000, "Carol"));
        set_raised_hand(&mut list, hand(10, 1_000, "Alice"));
        set_raised_hand(&mut list, hand(20, 2_000, "Bob"));
        let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Alice", "Bob", "Carol"]);
    }

    /// Equal `raised_at_ms` must break on `session_id`, so every participant
    /// renders the SAME order. Feeding the two hands in BOTH insertion orders
    /// and demanding an identical result is what proves the order does not
    /// depend on arrival.
    ///
    /// ADVERSARIAL (mutation): drop the `.then_with(|| a.session_id.cmp(...))`
    /// → `sort_by` is stable, so the two insertion orders yield DIFFERENT
    /// results → red.
    #[test]
    fn equal_timestamps_tie_break_on_session_id_identically_for_every_peer() {
        let mut a = Vec::new();
        set_raised_hand(&mut a, hand(77, 5_000, "Higher"));
        set_raised_hand(&mut a, hand(11, 5_000, "Lower"));

        let mut b = Vec::new();
        set_raised_hand(&mut b, hand(11, 5_000, "Lower"));
        set_raised_hand(&mut b, hand(77, 5_000, "Higher"));

        assert_eq!(a, b, "both participants must render the SAME order");
        assert_eq!(a[0].session_id, 11, "the lower session id goes first");
    }

    /// A forged `raised_at_ms = 0` jumps the queue. That is a DOCUMENTED,
    /// accepted property (the field is advisory; see the proto), and this test
    /// exists so a future reader does not mistake it for a bug — and so nobody
    /// "fixes" it by clamping, which would break legitimate re-announce of a
    /// hand raised minutes ago.
    #[test]
    fn a_forged_epoch_timestamp_jumps_the_queue_by_design() {
        let mut list = Vec::new();
        set_raised_hand(&mut list, hand(10, 1_000, "Honest"));
        set_raised_hand(&mut list, hand(20, 0, "Forger"));
        assert_eq!(list[0].name, "Forger");
        // The point: ordering only. Nothing here grants the forger any capability.
    }

    // ===================================================================
    // Level semantics + the redundant-write guard.
    // ===================================================================

    /// A re-announce of an unchanged hand must report "no change" so the caller
    /// skips the `Signal::write()` that would re-render every peer tile. This is
    /// the common case: one re-announce per joining peer.
    ///
    /// ADVERSARIAL (mutation): make the `if *existing == hand { return false; }`
    /// arm return `true` → red.
    #[test]
    fn a_redundant_reannounce_reports_no_change() {
        let mut list = Vec::new();
        assert!(set_raised_hand(&mut list, hand(10, 1_000, "Alice")));
        for _ in 0..20 {
            assert!(
                !set_raised_hand(&mut list, hand(10, 1_000, "Alice")),
                "an identical re-announce must not dirty the roster",
            );
        }
        assert_eq!(list.len(), 1, "and must never duplicate the entry");
    }

    /// A late-arriving display name (the cache lost the race with a
    /// re-announce) IS a change and must update in place, not append.
    ///
    /// ADVERSARIAL (mutation): replace the `*existing = hand;` update with a
    /// bare `return false;` → the placeholder name sticks → red.
    #[test]
    fn a_late_display_name_updates_in_place() {
        let mut list = Vec::new();
        set_raised_hand(&mut list, hand(10, 1_000, "Someone"));
        assert!(set_raised_hand(&mut list, hand(10, 1_000, "Alice")));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Alice");
    }

    /// A re-raise carries a NEW stamp, so the participant must move to the BACK
    /// of the queue. Updating in place without re-sorting would leave them
    /// wrongly at the front.
    ///
    /// ADVERSARIAL (mutation): delete the `sort_raised_hands(list)` call inside
    /// the update branch → Alice stays first → red.
    #[test]
    fn a_re_raise_moves_the_participant_to_the_back_of_the_queue() {
        let mut list = Vec::new();
        set_raised_hand(&mut list, hand(10, 1_000, "Alice"));
        set_raised_hand(&mut list, hand(20, 2_000, "Bob"));
        // Alice lowers and re-raises at t=3000.
        assert!(clear_raised_hand(&mut list, 10));
        assert!(set_raised_hand(&mut list, hand(10, 3_000, "Alice")));
        let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Bob", "Alice"]);

        // And the same holds WITHOUT an intervening lower (a re-announce that
        // carries a newer stamp because the peer re-raised while we were away).
        assert!(set_raised_hand(&mut list, hand(20, 4_000, "Bob")));
        let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["Alice", "Bob"]);
    }

    /// Clearing an absent session is a no-op that reports "no change" — the
    /// departure path fires for every leaver, and the overwhelming majority
    /// never raised a hand.
    ///
    /// ADVERSARIAL (mutation): make `clear_raised_hand` return `true`
    /// unconditionally → red.
    #[test]
    fn clearing_an_absent_session_reports_no_change() {
        let mut list = vec![hand(10, 1_000, "Alice")];
        assert!(!clear_raised_hand(&mut list, 999));
        assert_eq!(list.len(), 1);
        assert!(clear_raised_hand(&mut list, 10));
        assert!(list.is_empty());
    }

    // ===================================================================
    // Self entry: keyed on `is_self`, disjoint from the peer keyspace.
    // ===================================================================

    fn self_hand(session_id: u64, raised_at_ms: u64) -> RaisedHand {
        RaisedHand {
            session_id,
            raised_at_ms,
            name: "Me".to_string(),
            is_self: true,
        }
    }

    /// THE self-entry bug this keying exists to prevent: the local session id is
    /// resolved fresh on every toggle and can legitimately differ between the
    /// raise and the lower — it is `None` (→ the `u64::MAX` fallback) before
    /// SESSION_ASSIGNED and changes outright on a re-election. Keyed on the
    /// session id, the lower would find nothing and the local user's hand would
    /// stay up in their own banner with no way to take it down.
    ///
    /// ADVERSARIAL (mutation): change `clear_self_raised_hand` to
    /// `list.retain(|h| h.session_id != session_id)` taking an id → the lower
    /// misses and the assertion that the list is empty goes red.
    #[test]
    fn a_lower_clears_the_self_entry_even_after_the_session_id_changed() {
        let mut list = Vec::new();
        // Raised BEFORE SESSION_ASSIGNED, so the id is the u64::MAX fallback.
        assert!(set_self_raised_hand(&mut list, self_hand(u64::MAX, 1_000)));
        // A re-election later re-stamps it with the real id.
        assert!(resync_self_session_id(&mut list, 42));
        assert_eq!(list[0].session_id, 42);
        // The lower must still find it.
        assert!(clear_self_raised_hand(&mut list));
        assert!(list.is_empty(), "the local hand must always be lowerable");
    }

    /// A raise while an older self entry exists (a different session id) must
    /// REPLACE it, never leave two "You" rows in the banner.
    ///
    /// ADVERSARIAL (mutation): key `set_self_raised_hand`'s match on
    /// `h.session_id == hand.session_id` → the second raise appends → len is 2 →
    /// red.
    #[test]
    fn a_re_raise_after_a_session_change_replaces_the_self_entry() {
        let mut list = Vec::new();
        set_self_raised_hand(&mut list, self_hand(u64::MAX, 1_000));
        assert!(set_self_raised_hand(&mut list, self_hand(77, 2_000)));
        assert_eq!(list.len(), 1, "there is only ever ONE local participant");
        assert_eq!(list[0].session_id, 77);
        assert_eq!(list[0].raised_at_ms, 2_000);
    }

    /// The peer and self keyspaces are disjoint, so the `u64::MAX` fallback can
    /// never be clobbered by a peer whose relay-stamped random id happens to be
    /// `u64::MAX` — and, more importantly day to day, a departing peer's cleanup
    /// can never take the local user's hand down.
    ///
    /// ADVERSARIAL (mutation): drop the `h.is_self ||` guard in
    /// `clear_raised_hand`'s retain, or the `!h.is_self &&` in
    /// `set_raised_hand`'s match → one of the two assertions goes red.
    #[test]
    fn a_peer_can_never_clobber_or_clear_the_self_entry() {
        let mut list = Vec::new();
        set_self_raised_hand(&mut list, self_hand(u64::MAX, 1_000));
        // A peer whose id collides with the self fallback sentinel.
        assert!(set_raised_hand(
            &mut list,
            hand(u64::MAX, 2_000, "Impostor")
        ));
        assert_eq!(list.len(), 2, "the two keyspaces must not collide");
        assert!(list.iter().any(|h| h.is_self));

        // That peer departs. The local hand must survive.
        assert!(clear_raised_hand(&mut list, u64::MAX));
        assert_eq!(list.len(), 1);
        assert!(
            list[0].is_self,
            "a peer departure must never lower OUR hand"
        );
    }

    /// `resync_self_session_id` must not move the local user in the queue: the
    /// ordering key is `raised_at_ms`, and a re-election is not a re-raise.
    ///
    /// ADVERSARIAL (mutation): have `resync_self_session_id` also refresh
    /// `raised_at_ms` → the local user drops to the back of the queue on every
    /// reconnect → red.
    #[test]
    fn resync_keeps_the_local_queue_position() {
        let mut list = Vec::new();
        set_self_raised_hand(&mut list, self_hand(5, 1_000));
        set_raised_hand(&mut list, hand(20, 2_000, "Bob"));
        assert!(list[0].is_self, "we raised first");
        assert!(resync_self_session_id(&mut list, 999));
        assert!(
            list[0].is_self,
            "a re-election must not move us behind a later raiser"
        );
        assert_eq!(list[0].raised_at_ms, 1_000);
    }

    /// A no-op resync must report "no change" so it does not dirty the roster on
    /// every ordinary reconnect.
    #[test]
    fn resync_reports_no_change_when_the_session_id_is_unchanged() {
        let mut list = Vec::new();
        set_self_raised_hand(&mut list, self_hand(5, 1_000));
        assert!(!resync_self_session_id(&mut list, 5));
        // And no local hand at all is trivially a no-op.
        let mut empty: Vec<RaisedHand> = Vec::new();
        assert!(!resync_self_session_id(&mut empty, 5));
    }

    // ===================================================================
    // Copy.
    // ===================================================================

    #[test]
    fn banner_is_absent_when_no_hand_is_up() {
        assert_eq!(compose_raised_hands_banner(&[]), None);
    }

    /// The singular-self phrasing is its own branch because "You raised their
    /// hand" is what the generic path would produce.
    ///
    /// ADVERSARIAL (mutation): delete the `1 if list[0].is_self` arm → red.
    #[test]
    fn a_lone_self_hand_uses_second_person_copy() {
        let mut me = hand(10, 1_000, "Antonio");
        me.is_self = true;
        assert_eq!(
            compose_raised_hands_banner(&[me]),
            Some("You raised your hand".to_string())
        );
    }

    #[test]
    fn a_lone_peer_hand_uses_singular_copy() {
        assert_eq!(
            compose_raised_hands_banner(&[hand(10, 1_000, "Alice")]),
            Some("Alice raised their hand".to_string())
        );
    }

    /// OUR OWN hand is never spelled with our name on our own screen. Before
    /// this, a two-hand banner read "Antonio and Alice raised their hands" — the
    /// reader's own name, in the third person, on the reader's own display.
    ///
    /// Both halves of the fix are pinned here, because either alone is still
    /// wrong: rendering "You" without changing the possessive yields "You and
    /// Alice raised THEIR hands".
    ///
    /// ADVERSARIAL (mutation): drop the `if h.is_self` arm in `join_names`'s
    /// `map` → "Me and Alice ..." → red. Hardcode the possessive back to
    /// "their" → red.
    #[test]
    fn a_shared_banner_addresses_us_in_the_second_person() {
        let mut me = hand(10, 1_000, "Me");
        me.is_self = true;
        assert_eq!(
            compose_raised_hands_banner(&[me.clone(), hand(20, 2_000, "Alice")]),
            Some("You and Alice raised your hands".to_string()),
        );
        // Queue order is preserved, so we are named wherever we actually are.
        let mut later = hand(30, 3_000, "Me");
        later.is_self = true;
        assert_eq!(
            compose_raised_hands_banner(&[hand(20, 2_000, "Alice"), later]),
            Some("Alice and You raised your hands".to_string()),
        );
    }

    /// The subtle half: once our hand falls PAST the spelled-out names it is no
    /// longer part of the sentence's subject, so the sentence goes back to the
    /// third person. "Alice, Bob, Carol and 1 other raised YOUR hands" is the
    /// bug a roster-wide `any(is_self)` test would ship.
    ///
    /// ADVERSARIAL (mutation): drop the `.take(RAISED_HANDS_BANNER_MAX_NAMES)`
    /// from `self_is_named` → "your hands" → red.
    #[test]
    fn a_self_hand_hidden_in_the_overflow_keeps_the_third_person() {
        let mut me = hand(40, 4_000, "Me");
        me.is_self = true;
        assert_eq!(
            compose_raised_hands_banner(&[
                hand(10, 1_000, "Alice"),
                hand(20, 2_000, "Bob"),
                hand(30, 3_000, "Carol"),
                me,
            ]),
            Some("Alice, Bob, Carol and 1 other raised their hands".to_string()),
        );
    }

    /// The screen-reader summary shares `join_names`, so it inherits the
    /// second-person rendering — a blind user must not hear their own name read
    /// back at them either.
    ///
    /// ADVERSARIAL (mutation): drop the `if h.is_self` arm in `join_names`'s
    /// `map` → the utterance names "Me" → red.
    #[test]
    fn the_announcement_names_us_in_the_second_person() {
        let mut me = hand(10, 1_000, "Me");
        me.is_self = true;
        assert_eq!(
            raised_hands_live_text(&[me, hand(20, 2_000, "Alice")]),
            "2 hands raised. You and Alice.",
        );
    }

    /// ONE serial-comma convention across both `join_names` branches. They used
    /// to disagree — "Alice, Bob and Carol" against "Alice, Bob, Carol, and 2
    /// others" — which is visible to a user the moment a fourth hand goes up.
    ///
    /// ADVERSARIAL (mutation): restore the `", and {}"` format in the overflow
    /// arm → the two phrasings disagree again → red.
    #[test]
    fn both_list_forms_use_the_same_serial_comma_convention() {
        let names = ["Alice", "Bob", "Carol", "Dan"];
        let three: Vec<RaisedHand> = names[..3]
            .iter()
            .enumerate()
            .map(|(i, n)| hand(i as u64 * 10, i as u64, n))
            .collect();
        let four: Vec<RaisedHand> = names
            .iter()
            .enumerate()
            .map(|(i, n)| hand(i as u64 * 10, i as u64, n))
            .collect();
        assert_eq!(join_names(&three), "Alice, Bob and Carol");
        assert_eq!(join_names(&four), "Alice, Bob, Carol and 1 other");
    }

    #[test]
    fn two_hands_read_as_a_pair() {
        assert_eq!(
            compose_raised_hands_banner(&[hand(10, 1, "Alice"), hand(20, 2, "Bob")]),
            Some("Alice and Bob raised their hands".to_string())
        );
    }

    #[test]
    fn three_hands_read_as_a_comma_list() {
        assert_eq!(
            compose_raised_hands_banner(&[
                hand(10, 1, "Alice"),
                hand(20, 2, "Bob"),
                hand(30, 3, "Carol"),
            ]),
            Some("Alice, Bob and Carol raised their hands".to_string())
        );
    }

    /// The overflow collapse is what stops the banner covering the video in a
    /// large meeting — the exact moment the feature matters most.
    ///
    /// ADVERSARIAL (mutation): raise `RAISED_HANDS_BANNER_MAX_NAMES` to 10 (or
    /// delete the `.take(...)`) → every name is spelled out → red.
    #[test]
    fn more_than_three_hands_collapse_into_and_n_others() {
        let list: Vec<RaisedHand> = ["Alice", "Bob", "Carol", "Dan", "Erin"]
            .iter()
            .enumerate()
            .map(|(i, n)| hand(i as u64 * 10, i as u64, n))
            .collect();
        assert_eq!(
            compose_raised_hands_banner(&list),
            Some("Alice, Bob, Carol and 2 others raised their hands".to_string())
        );
    }

    /// Exactly one overflow name is singular ("1 other", not "1 others").
    #[test]
    fn a_single_overflow_name_is_singular() {
        let list: Vec<RaisedHand> = ["Alice", "Bob", "Carol", "Dan"]
            .iter()
            .enumerate()
            .map(|(i, n)| hand(i as u64 * 10, i as u64, n))
            .collect();
        assert_eq!(
            compose_raised_hands_banner(&list),
            Some("Alice, Bob, Carol and 1 other raised their hands".to_string())
        );
    }

    /// Only the REACHABLE inputs are pinned. The dropped `compose_raised_hands_
    /// count(0)` assertion described a string no call site can produce (see that
    /// function's doc), so it asserted behaviour nothing could regress.
    #[test]
    fn count_copy_is_singular_for_one() {
        assert_eq!(compose_raised_hands_count(1), "1 hand raised");
        assert_eq!(compose_raised_hands_count(2), "2 hands raised");
    }

    // ===================================================================
    // Compact (mobile) copy.
    // ===================================================================

    /// The narrow-viewport form must answer WHO, not just how many. Hiding the
    /// names below 639px left mobile users with "3 hands raised" and no way to
    /// learn whose — on the devices where the roster drawer and the tile badges
    /// are hardest to reach, and "who" is the literal ask in the issue.
    ///
    /// ADVERSARIAL (mutation): make `compose_raised_hands_compact` delegate to
    /// `compose_raised_hands_count` (the old behaviour) → no name appears → red.
    #[test]
    fn the_compact_form_names_the_head_of_the_queue() {
        assert_eq!(
            compose_raised_hands_compact(&[hand(10, 1, "Alice")]).as_deref(),
            Some("Alice"),
        );
        assert_eq!(
            compose_raised_hands_compact(&[
                hand(10, 1, "Alice"),
                hand(20, 2, "Bob"),
                hand(30, 3, "Carol"),
            ])
            .as_deref(),
            Some("Alice +2"),
            "the first raiser by name, and how many are queued behind them",
        );
    }

    /// Our own hand reads in the second person here too — "Antonio +2" about
    /// yourself reads as somebody else.
    ///
    /// ADVERSARIAL (mutation): drop the `if first.is_self` branch → "Me +1" → red.
    #[test]
    fn the_compact_form_uses_second_person_for_our_own_hand() {
        let mut me = hand(10, 1, "Me");
        me.is_self = true;
        assert_eq!(
            compose_raised_hands_compact(&[me.clone()]).as_deref(),
            Some("You"),
        );
        assert_eq!(
            compose_raised_hands_compact(&[me, hand(20, 2, "Bob")]).as_deref(),
            Some("You +1"),
        );
    }

    /// Empty roster → nothing, mirroring the full sentence: with no hand up the
    /// banner does not render at all, so there is no compact form either.
    #[test]
    fn the_compact_form_is_absent_when_no_hand_is_up() {
        assert_eq!(compose_raised_hands_compact(&[]), None);
    }

    // ===================================================================
    // Screen-reader copy.
    // ===================================================================

    /// One utterance summarises the CURRENT roster — the property that keeps a
    /// 20-person wave from becoming 20 utterances.
    ///
    /// ADVERSARIAL (mutation): make `compose_raised_hands_announcement` return
    /// only the first name → the count disappears → red.
    #[test]
    fn the_announcement_summarises_state_not_events() {
        let list: Vec<RaisedHand> = (0..20)
            .map(|i| hand(i as u64, i as u64, &format!("P{i}")))
            .collect();
        assert_eq!(
            compose_raised_hands_announcement(&list),
            Some("20 hands raised. P0, P1, P2 and 17 others.".to_string()),
        );
    }

    /// An emptied roster must be announced EXPLICITLY. Clearing a live region
    /// announces nothing, so a screen-reader user would otherwise never learn
    /// the queue drained.
    ///
    /// ADVERSARIAL (mutation): make `raised_hands_live_text` return
    /// `String::new()` for the empty case → red.
    #[test]
    fn a_drained_roster_announces_all_hands_lowered() {
        assert_eq!(compose_raised_hands_announcement(&[]), None);
        assert_eq!(raised_hands_live_text(&[]), "All hands lowered");
        assert_eq!(
            raised_hands_live_text(&[hand(10, 1, "Alice")]),
            "1 hand raised. Alice."
        );
    }

    /// Entering a meeting must NOT announce "All hands lowered" — the banner
    /// mounts with an empty roster, and its first throttle window would
    /// otherwise speak about hands nobody ever raised.
    ///
    /// ADVERSARIAL (mutation): delete the `if last_announced.is_empty() &&
    /// roster_empty { return false; }` arm → the mount case returns true → red.
    #[test]
    fn entering_a_meeting_announces_nothing() {
        assert!(!should_announce_roster("", true, "All hands lowered"));
    }

    /// A raise whose matching lower lands inside the SAME throttle window nets to
    /// zero and must stay silent — announcing the lowering of a raise the user
    /// never heard is worse than saying nothing.
    ///
    /// This is the case an ARM-TIME guard alone cannot cover, and getting it
    /// wrong is invisible in review: the effect that observes the lower cannot
    /// un-arm the timer the raise already started, so the write-time check is the
    /// only thing standing between the user and a phantom announcement.
    ///
    /// ADVERSARIAL (mutation): same as above — the empty/empty arm is what makes
    /// this false.
    #[test]
    fn a_raise_and_lower_inside_one_throttle_window_stays_silent() {
        // The timer was armed by the raise; by the time it fires the roster has
        // drained again and nothing has ever been announced.
        assert!(!should_announce_roster("", true, "All hands lowered"));
    }

    /// Once something HAS been announced, a drain must speak — the whole point of
    /// the explicit "All hands lowered" text.
    ///
    /// ADVERSARIAL (mutation): widen the suppression to `if roster_empty { return
    /// false; }` (dropping the `last_announced.is_empty() &&`) → a genuine drain
    /// goes silent → red.
    #[test]
    fn a_drain_after_a_real_announcement_does_speak() {
        assert!(should_announce_roster(
            "1 hand raised. Alice.",
            true,
            "All hands lowered"
        ));
    }

    /// An unchanged summary is not re-written: the DOM text would be identical
    /// (so nothing is re-announced anyway) and the write would dirty the
    /// component for nothing.
    ///
    /// ADVERSARIAL (mutation): change the final `last_announced != next` to
    /// `true` → red.
    #[test]
    fn an_unchanged_summary_is_not_rewritten() {
        assert!(!should_announce_roster(
            "1 hand raised. Alice.",
            false,
            "1 hand raised. Alice."
        ));
        assert!(should_announce_roster(
            "1 hand raised. Alice.",
            false,
            "2 hands raised. Alice and Bob."
        ));
    }

    /// The FIRST raise of a session speaks, even though nothing was announced
    /// before it — the suppression must be keyed on the roster being empty too,
    /// not on "nothing announced yet" alone.
    ///
    /// ADVERSARIAL (mutation): drop the `&& roster_empty` → the first raise of
    /// every meeting is silent → red.
    #[test]
    fn the_first_raise_of_a_session_speaks() {
        assert!(should_announce_roster("", false, "1 hand raised. Alice."));
    }

    /// The roster badge's accessible name carries the queue POSITION AND THE
    /// TOTAL, and does NOT repeat the participant's name.
    ///
    /// Both halves are corrections. "(2 in the queue)" reads as a count — "there
    /// are 2 people in the queue" — which is the opposite of what it means; and
    /// the badge renders inside the row that already begins with the name, so
    /// including it made AT read "Alice … Alice raised their hand".
    ///
    /// ADVERSARIAL (mutation): drop the `of {total}` → red; re-introduce a
    /// leading `{name} ` → red.
    #[test]
    fn the_badge_label_states_the_queue_slot_without_repeating_the_name() {
        assert_eq!(
            raised_hand_badge_label(Some((2, 5))),
            "Hand raised, position 2 of 5"
        );
        assert_eq!(raised_hand_badge_label(None), "Hand raised");
    }

    // ===================================================================
    // The `would_*_change` predicates that keep the inbound path from
    // cloning the roster per packet.
    // ===================================================================

    /// THE PROPERTY THAT MATTERS: each predicate must agree with the mutator it
    /// guards, for every shape the inbound path can produce. A predicate that
    /// said "no change" where the mutator would have changed something drops a
    /// real update; one that said "change" where the mutator would not
    /// reinstates the per-packet clone it exists to avoid.
    ///
    /// This calls BOTH production functions and compares their answers — it does
    /// not re-derive either. `is_peer_entry` is shared by both, so this pins that
    /// sharing rather than a copy of it.
    ///
    /// ADVERSARIAL (mutation): drop the `!h.is_self &&` from `is_peer_entry`, or
    /// make `would_set_raised_hand_change` return `existing == hand`, or have
    /// either predicate ignore the self entry differently from its mutator → the
    /// two answers diverge on one of the rows below → red.
    #[test]
    fn each_predicate_agrees_with_the_mutator_it_guards() {
        let mut me = hand(u64::MAX, 500, "Me");
        me.is_self = true;
        let base = vec![me, hand(10, 1_000, "Alice"), hand(20, 2_000, "Bob")];

        // Every inbound RAISE shape: identical re-announce, changed name,
        // changed stamp, brand-new session, and a peer id colliding with the
        // self-entry sentinel.
        let raises = [
            hand(10, 1_000, "Alice"),  // redundant re-announce
            hand(10, 1_000, "Alicia"), // late display name
            hand(10, 3_000, "Alice"),  // re-raise, new stamp
            hand(99, 4_000, "Newcomer"),
            hand(u64::MAX, 4_000, "Impostor"), // collides with the self sentinel
        ];
        for incoming in raises {
            let predicted = would_set_raised_hand_change(&base, &incoming);
            let mut actual_list = base.clone();
            let actual = set_raised_hand(&mut actual_list, incoming.clone());
            assert_eq!(
                predicted, actual,
                "would_set_raised_hand_change disagreed with set_raised_hand for {incoming:?}",
            );
        }

        // Every inbound LOWER / departure shape: present peer, absent session,
        // and the self sentinel (which the peer path must never touch).
        for session_id in [10, 20, 999, u64::MAX] {
            let predicted = would_clear_raised_hand_change(&base, session_id);
            let mut actual_list = base.clone();
            let actual = clear_raised_hand(&mut actual_list, session_id);
            assert_eq!(
                predicted, actual,
                "would_clear_raised_hand_change disagreed with clear_raised_hand for {session_id}",
            );
        }
    }

    /// The case the predicates exist for, stated on its own so it cannot be lost
    /// in the agreement matrix: a redundant re-announce — the COMMON inbound
    /// packet, one per joining peer per raised hand — must be answerable without
    /// touching the list, and must answer "no".
    ///
    /// ADVERSARIAL (mutation): `would_set_raised_hand_change` returning `true`
    /// unconditionally → red.
    #[test]
    fn a_redundant_reannounce_is_rejected_before_any_clone() {
        let list = vec![hand(10, 1_000, "Alice")];
        assert!(!would_set_raised_hand_change(
            &list,
            &hand(10, 1_000, "Alice")
        ));
        // ...and a departure for a participant who never raised, which fires for
        // the overwhelming majority of leavers.
        assert!(!would_clear_raised_hand_change(&list, 999));
    }
    // ===================================================================
    // The raise/lower chime policy (issue 2329).
    //
    // Every test below calls the PRODUCTION `hand_sound_to_play` — the same
    // entry point both call sites in `attendants.rs` use — rather than
    // re-deriving the gate arithmetic, so a regression in any gate fails here.
    //
    // THE PAIR THAT MATTERS is `a_hand_already_up_when_we_joined_is_silent` and
    // `a_peer_raising_while_we_watch_chimes`. They are deliberately identical in
    // every respect a state-based rule could see — same peer, same
    // `false -> true`, same first observation of that peer's hand — and differ
    // ONLY in whether the raise predates our connect. Two earlier designs (a
    // window on our own connect, then a window on each peer's relay
    // introduction) each passed one and failed the other end to end, 5/5. Keep
    // both, and keep them adjacent.
    // ===================================================================

    /// A channel that joined at `connected_at_ms` and has never spoken.
    fn joined_at(connected_at_ms: f64) -> HandSoundChannel {
        HandSoundChannel {
            connected_at_ms: Some(connected_at_ms),
            ..HandSoundChannel::default()
        }
    }

    /// `raised_at_ms` for a hand raised `ms` AFTER we joined at `JOINED`.
    const JOINED: f64 = 1_000_000.0;
    fn raised_after_join(ms: f64) -> f64 {
        JOINED + HAND_SOUND_CLOCK_SKEW_MARGIN_MS + ms
    }

    /// THE ISSUE 2276 HAZARD: arriving in a room where hands are already up must
    /// cost ZERO chimes — not one per hand, and not one coalesced chime for
    /// hands nobody just raised.
    ///
    /// The arrival times are spread across a wide range on purpose, including
    /// far beyond any window either previous design used, because the whole
    /// point of comparing stamps is that WHEN the packet arrives is irrelevant.
    /// The rate gate cannot be what makes these silent either: the channel never
    /// speaks, so `last_played_ms` stays `NEG_INFINITY` and it is open at every
    /// one of them.
    ///
    /// ADVERSARIAL (mutation): delete the `raise_happened_after_we_joined` arm
    /// from `hand_sound_to_play`, or make that function return `true`
    /// unconditionally → all five chime → red.
    #[test]
    fn a_hand_already_up_when_we_joined_is_silent() {
        let channel = joined_at(JOINED);
        // Raised well before we joined — a minute, a second, and right at the
        // boundary.
        let raised_at = JOINED - 60_000.0;
        for arrival in [
            JOINED + 100.0,
            JOINED + 3_000.0,
            JOINED + 30_000.0,
            JOINED + 600_000.0,
            JOINED + 3_600_000.0,
        ] {
            assert_eq!(
                hand_sound_to_play(channel, raised_at, true, false, arrival, false, true),
                None,
                "a replay arriving {arrival}ms after we joined must still be silent",
            );
        }
    }

    /// The other half of the pair: a peer raising while we watch MUST chime.
    ///
    /// Identical to the test above in peer, transition and first-observation
    /// status — only `raised_at_ms` differs. This is the case both previous
    /// window designs would break if the storm gate were widened to fix the
    /// other one, and the case a "first packet establishes state" rule would
    /// break outright (a peer with a hand DOWN sends nothing, so their eventual
    /// raise IS their first hand packet).
    ///
    /// ADVERSARIAL (mutation): make `raise_happened_after_we_joined` return
    /// `false` unconditionally → red.
    #[test]
    fn a_peer_raising_while_we_watch_chimes() {
        let channel = joined_at(JOINED);
        let raised_at = raised_after_join(1.0);
        assert_eq!(
            hand_sound_to_play(
                channel,
                raised_at,
                true,
                false,
                raised_at + 900.0,
                false,
                true
            ),
            Some(HandSound::Raised),
        );
        // ...and still chimes hours later, which is what makes this independent
        // of how long ago we joined.
        let much_later = raised_after_join(3_600_000.0);
        assert_eq!(
            hand_sound_to_play(
                channel,
                much_later,
                true,
                false,
                much_later + 900.0,
                false,
                true
            ),
            Some(HandSound::Raised),
        );
    }

    /// A LOWER is never gated on the stamp, and must not be.
    ///
    /// `raised_at_ms` is documented meaningless (`0`) when `raised == false`, and
    /// a peer whose hand is DOWN never re-announces — so there is no such thing
    /// as a replayed lower. Feeding lowers through the storm gate would silence
    /// every genuine lower in the room, including the one the e2e's own positive
    /// control asserts.
    ///
    /// The stamp used here is `0.0`, i.e. wildly "before we joined": if the
    /// storm gate applied to lowers, this would be suppressed.
    ///
    /// ADVERSARIAL (mutation): drop the `sound == HandSound::Raised &&` guard so
    /// the storm gate covers both directions → red.
    #[test]
    fn a_lower_is_never_gated_on_the_raise_stamp() {
        let channel = joined_at(JOINED);
        assert_eq!(
            hand_sound_to_play(channel, 0.0, true, false, JOINED + 30_000.0, true, false),
            Some(HandSound::Lowered),
        );
    }

    /// Before we have ever connected the channel is MUTE: with no idea when we
    /// joined, nothing can be shown to have happened after it.
    ///
    /// ADVERSARIAL (mutation): make `raise_happened_after_we_joined` return
    /// `true` for the `None` arm → red.
    #[test]
    fn an_inbound_raise_before_our_first_connect_is_mute() {
        assert_eq!(
            hand_sound_to_play(
                HandSoundChannel::default(),
                f64::MAX,
                true,
                false,
                1_000.0,
                false,
                true
            ),
            None,
        );
    }

    /// A RECONNECT re-stamps the reference instant, which is what makes the
    /// post-blip replay free.
    ///
    /// The member-list replay tears every raised hand down
    /// (`OnPeerLeftAction::ClearRaisedHand` runs BEFORE the reconnect
    /// early-return) and every peer re-announces, so without this one transport
    /// blip in a hands-up meeting is a full storm. Note the hand here was raised
    /// AFTER the original join — it legitimately chimed the first time — and must
    /// not chime again on the replay.
    ///
    /// ADVERSARIAL (mutation): stamp `connected_at_ms` only on the FIRST connect
    /// → the replayed raise still reads as "after we joined" → red.
    #[test]
    fn a_reconnect_re_stamps_the_reference_instant() {
        let raised_at = raised_after_join(1_000.0);
        // First time round it speaks.
        assert_eq!(
            hand_sound_to_play(
                joined_at(JOINED),
                raised_at,
                true,
                false,
                raised_at + 900.0,
                false,
                true
            ),
            Some(HandSound::Raised),
        );
        // We blip; `on_connected` re-stamps. The same hand is torn down and
        // re-announced, and must now be silent.
        let reconnect = raised_at + 600_000.0;
        assert_eq!(
            hand_sound_to_play(
                joined_at(reconnect),
                raised_at,
                true,
                false,
                reconnect + 900.0,
                false,
                true
            ),
            None,
        );
        // ...while a genuine raise after the reconnect still speaks.
        let after = reconnect + HAND_SOUND_CLOCK_SKEW_MARGIN_MS + 1.0;
        assert_eq!(
            hand_sound_to_play(
                joined_at(reconnect),
                after,
                true,
                false,
                after + 900.0,
                false,
                true
            ),
            Some(HandSound::Raised),
        );
    }

    /// A RE-ANNOUNCE of a hand we already hold up is not a raise, whatever its
    /// stamp says. It is the COMMON inbound packet — one per joining peer per
    /// raised hand, broadcast to the whole room — so getting this wrong makes
    /// every join a chime for everyone, not just the joiner.
    ///
    /// The late-display-name case is called out separately because it is the one
    /// that survives `would_set_raised_hand_change`: the roster genuinely
    /// CHANGES (the name updates in place) while the level does not.
    ///
    /// ADVERSARIAL (mutation): make `hand_sound_for_transition` key on
    /// `now_raised` alone → red.
    #[test]
    fn a_reannounce_of_an_already_raised_hand_is_silent() {
        let channel = joined_at(JOINED);
        let fresh = raised_after_join(1.0);
        assert_eq!(
            hand_sound_to_play(channel, fresh, true, false, fresh + 900.0, true, true),
            None,
            "an unchanged re-announce is not a raise, even with a fresh stamp",
        );
        assert_eq!(
            hand_sound_to_play(channel, fresh, true, false, fresh + 60_000.0, true, true),
            None,
            "and neither is one that only resolves a late display name",
        );
        // The mirror: a lower for a hand we never had up.
        assert_eq!(
            hand_sound_to_play(channel, 0.0, true, false, JOINED + 9_000.0, false, false),
            None,
        );
    }

    /// THE SKEW BOUNDARY, pinned so nobody widens
    /// [`HAND_SOUND_CLOCK_SKEW_MARGIN_MS`] without seeing what it buys and costs.
    ///
    /// Both directions are asserted because they trade directly against each
    /// other: every millisecond of tolerance for a FAST sender clock is a
    /// millisecond after our own connect during which a genuine raise from an
    /// honest clock is silent.
    ///
    /// ADVERSARIAL (mutation): change the `>` to `>=`, or drop the
    /// `+ HAND_SOUND_CLOCK_SKEW_MARGIN_MS`, and the first two assertions
    /// disagree → red.
    #[test]
    fn the_skew_margin_is_the_only_tolerance_and_it_is_bounded() {
        let channel = joined_at(JOINED);
        // Exactly AT the margin is still suppressed — the comparison is strict.
        assert_eq!(
            hand_sound_to_play(
                channel,
                JOINED + HAND_SOUND_CLOCK_SKEW_MARGIN_MS,
                true,
                false,
                JOINED + 10_000.0,
                false,
                true
            ),
            None,
        );
        // One millisecond past it speaks.
        assert_eq!(
            hand_sound_to_play(
                channel,
                JOINED + HAND_SOUND_CLOCK_SKEW_MARGIN_MS + 1.0,
                true,
                false,
                JOINED + 10_000.0,
                false,
                true
            ),
            Some(HandSound::Raised),
        );
    }

    /// THE PROPERTY THAT KILLED THE BAND DESIGN: a badly skewed sender is NOT
    /// silenced forever.
    ///
    /// Under a "was the raise recent?" band, a peer whose clock is off by more
    /// than the band never chimes again for the rest of the meeting — a silent,
    /// undiagnosable feature for that person, and strictly worse than the storm
    /// it was fixing. Under the boundary comparison the penalty is confined to
    /// raises near our own connect: a peer five minutes slow is silent only for
    /// raises in the first five minutes, and correct forever after.
    ///
    /// ADVERSARIAL (mutation): reintroduce an upper bound — gate on
    /// `now_ms - raised_at_ms <= SOME_BAND` as well — and the second assertion
    /// goes red.
    #[test]
    fn a_badly_skewed_sender_is_not_muted_forever() {
        let channel = joined_at(JOINED);
        let skew = 300_000.0; // five minutes slow

        // Early on, their raise is lost. That is the accepted cost.
        let early_real = JOINED + 60_000.0;
        assert_eq!(
            hand_sound_to_play(
                channel,
                early_real - skew,
                true,
                false,
                early_real,
                false,
                true
            ),
            None,
            "a raise inside the skew of our connect is the accepted cost",
        );

        // Later, the SAME badly-skewed peer is heard correctly — and stays heard.
        let late_real = JOINED + 3_600_000.0;
        assert_eq!(
            hand_sound_to_play(
                channel,
                late_real - skew,
                true,
                false,
                late_real,
                false,
                true
            ),
            Some(HandSound::Raised),
            "but the same peer must not be muted for the rest of the meeting",
        );
    }

    /// The preference gates BOTH directions.
    ///
    /// The enabled assertions use the SAME inputs, so this cannot pass
    /// vacuously: a mutation that silenced everything would fail the second half.
    ///
    /// ADVERSARIAL (mutation): delete the `if !enabled { return false; }` arm →
    /// the two disabled assertions go red.
    #[test]
    fn the_preference_gates_both_directions() {
        let channel = joined_at(JOINED);
        let fresh = raised_after_join(1.0);
        let t = fresh + 900.0;
        assert_eq!(
            hand_sound_to_play(channel, fresh, false, false, t, false, true),
            None,
            "a raise must be silent with the preference off",
        );
        assert_eq!(
            hand_sound_to_play(channel, 0.0, false, false, t, true, false),
            None,
            "and so must a lower",
        );
        assert!(hand_sound_to_play(channel, fresh, true, false, t, false, true).is_some());
        assert!(hand_sound_to_play(channel, 0.0, true, false, t, true, false).is_some());
    }

    /// The LOCAL user's own toggle skips the storm gate but NOT the rate gate.
    ///
    /// The exemption is not a loophole: the storm gate exists to suppress
    /// INBOUND replay, and a local press can never be replay — it is a user
    /// action that happened just now, by construction. Note the stamp used here
    /// is `0.0`, which for a REMOTE peer would be suppressed.
    ///
    /// ADVERSARIAL (mutation): drop the `!is_self &&` from the storm arm → the
    /// first assertion goes red. Delete the rate gate → the second goes red.
    #[test]
    fn the_local_toggle_skips_the_storm_gate_but_not_the_rate_gate() {
        let channel = joined_at(JOINED);
        assert_eq!(
            hand_sound_to_play(channel, 0.0, true, true, JOINED + 100.0, false, true),
            Some(HandSound::Raised),
            "our own press is never replay, whatever the stamp says",
        );
        let just_spoke = HandSoundChannel {
            connected_at_ms: Some(JOINED),
            last_played_ms: JOINED + 100.0,
        };
        assert_eq!(
            hand_sound_to_play(just_spoke, 0.0, true, true, JOINED + 200.0, true, false),
            None,
            "but it must not smear over a chime the channel played 100ms ago",
        );
    }

    /// THE Q&A CASE: ten hands going up over ~2 s must not become ten
    /// overlapping tone pairs — which is not merely mush, since `play_tone_pair`
    /// builds a new `AudioContext` per chime and browsers cap how many a tab may
    /// hold.
    ///
    /// Drives the gate the way production does, feeding back `last_played_ms`
    /// only when a chime actually played, exactly as `maybe_play_hand_sound`
    /// does. Every hand here is a GENUINE post-join raise, so the storm gate is
    /// not what is being measured.
    ///
    /// ADVERSARIAL (mutation): set `HAND_SOUND_MIN_INTERVAL_MS` to 0.0 → all ten
    /// play → red. Raise it to 5000 → one plays → red.
    #[test]
    fn a_wave_of_hands_collapses_to_one_chime_per_rate_window() {
        let mut channel = joined_at(JOINED);
        let base = raised_after_join(1.0);
        let mut played = 0;
        for i in 0..10 {
            let now = base + f64::from(i) * 200.0;
            if hand_sound_to_play(channel, now, true, false, now, false, true).is_some() {
                played += 1;
                channel.last_played_ms = now;
            }
        }
        // Ten raises 200ms apart against a 600ms floor: t=0, 600, 1200, 1800.
        assert_eq!(played, 4, "the wave must collapse, not stack");
    }

    /// ONE rate gate for both directions. Two independent gates could still
    /// overlap with each other, which is the smear this exists to prevent.
    ///
    /// ADVERSARIAL (mutation): give the raise and lower directions separate
    /// watermarks → the first assertion goes red.
    #[test]
    fn the_rate_gate_is_shared_across_both_directions() {
        let spoke_at = JOINED + 50_000.0;
        let just_raised = HandSoundChannel {
            connected_at_ms: Some(JOINED),
            last_played_ms: spoke_at,
        };
        assert_eq!(
            hand_sound_to_play(just_raised, 0.0, true, false, spoke_at + 80.0, true, false),
            None,
            "a lower 80ms after a raise must not sound on top of it",
        );
        assert_eq!(
            hand_sound_to_play(
                just_raised,
                0.0,
                true,
                false,
                spoke_at + HAND_SOUND_MIN_INTERVAL_MS,
                true,
                false
            ),
            Some(HandSound::Lowered),
            "and must speak once the shared window closes",
        );
    }

    /// The rate gate must not WEDGE on a backwards wall-clock step (NTP
    /// correction, laptop resume). A bare `elapsed >= N` reads a negative elapsed
    /// as "too soon" and would mute hand audio for the whole size of the step,
    /// with nothing to re-open it.
    ///
    /// ADVERSARIAL (mutation): drop the half-open range in
    /// `hand_sound_channel_open` for a bare `>=` → red.
    #[test]
    fn a_backwards_wall_clock_step_cannot_wedge_the_rate_gate() {
        let now = JOINED + 10_000.0;
        let spoke_in_the_future = HandSoundChannel {
            connected_at_ms: Some(JOINED),
            last_played_ms: now + 3_600_000.0,
        };
        assert_eq!(
            hand_sound_to_play(spoke_in_the_future, 0.0, true, false, now, true, false),
            Some(HandSound::Lowered),
        );
    }
}
