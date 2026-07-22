// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure, host-testable UI logic for meeting reactions (issue #1884).
//!
//! Two concerns live here, both DOM/clock-free so they run under a plain native
//! `#[test]`:
//!   1. the enum→(emoji, label, slug) table — the CLIENT-side single source of
//!      truth for how a wire `ReactionType` renders (spec §1), and
//!   2. the floating-overlay integration logic — coalescing repeats of the same
//!      (sender, reaction) and enforcing the hard concurrency cap (spec §4d).
//!
//! The Dioxus rendering (palette + rising-emoji overlay) in `attendants.rs`
//! calls these; it does not re-implement the table or the cap/coalesce math.

use videocall_types::protos::reaction_packet::reaction_packet::ReactionType;

/// The 7 user-selectable reactions, in palette order. `ReactionType` is a plain
/// C-like enum (`value()` == `*self as i32`), so callers convert to the wire i32
/// with `reaction as i32` where needed.
pub const REACTIONS: [ReactionType; 7] = [
    ReactionType::THUMBS_UP,
    ReactionType::THUMBS_DOWN,
    ReactionType::LAUGH,
    ReactionType::APPLAUSE,
    ReactionType::HEART,
    ReactionType::THINKING,
    ReactionType::PARTY,
];

/// Step the palette highlight by `delta` positions through [`REACTIONS`] with
/// wraparound (issue #1884). ArrowRight / ArrowDown pass `+1`; ArrowLeft /
/// ArrowUp pass `-1`; Home/End jump to the ends directly. A `current` not found
/// in [`REACTIONS`] resets to the first entry, so the highlight can never wedge
/// on a stale value.
pub fn step_reaction(current: ReactionType, delta: i32) -> ReactionType {
    let n = REACTIONS.len() as i32;
    let idx = REACTIONS
        .iter()
        .position(|&r| r == current)
        .map(|i| i as i32)
        .unwrap_or(0);
    // Rust `%` can be negative; the extra `+ n) % n` folds it back to [0, n).
    let next = (((idx + delta) % n) + n) % n;
    REACTIONS[next as usize]
}

/// Client-side single source of truth: map a reaction to its
/// `(emoji, human label, dom slug)`. `REACTION_TYPE_UNSPECIFIED` (and, via
/// [`reaction_glyph_from_i32`], any unknown/reserved wire value) maps to `None`
/// so the caller renders nothing rather than a blank/placeholder glyph. The
/// glyphs are native Unicode emoji; the label is the accessible name
/// ("React with {label}"); the slug is the DOM/testid token.
pub fn reaction_glyph(
    reaction: ReactionType,
) -> Option<(&'static str, &'static str, &'static str)> {
    match reaction {
        ReactionType::THUMBS_UP => Some(("👍", "thumbs up", "thumbs_up")),
        ReactionType::THUMBS_DOWN => Some(("👎", "thumbs down", "thumbs_down")),
        ReactionType::LAUGH => Some(("😂", "laughing", "laugh")),
        ReactionType::APPLAUSE => Some(("👏", "applause", "applause")),
        ReactionType::HEART => Some(("❤️", "heart", "heart")),
        ReactionType::THINKING => Some(("🤔", "thinking", "thinking")),
        ReactionType::PARTY => Some(("🎉", "party", "party")),
        ReactionType::REACTION_TYPE_UNSPECIFIED => None,
    }
}

/// Map a raw wire reaction value (as delivered to the `on_reaction` callback —
/// `ReactionPacket.reaction.value()`) to its glyph. `0` (UNSPECIFIED) and any
/// out-of-range / reserved value (8..=31, negatives, junk) map to `None`,
/// matching the relay's closed-enum allowlist. The `1..=7` arm delegates to
/// [`reaction_glyph`], so the two functions can never disagree (pinned by test).
pub fn reaction_glyph_from_i32(value: i32) -> Option<(&'static str, &'static str, &'static str)> {
    let reaction = match value {
        1 => ReactionType::THUMBS_UP,
        2 => ReactionType::THUMBS_DOWN,
        3 => ReactionType::LAUGH,
        4 => ReactionType::APPLAUSE,
        5 => ReactionType::HEART,
        6 => ReactionType::THINKING,
        7 => ReactionType::PARTY,
        _ => return None,
    };
    reaction_glyph(reaction)
}

/// Coalesce window (issue #1884): a repeat of the SAME (sender, reaction) within
/// this many ms of the existing float's birth increments its count instead of
/// spawning a second float.
pub const REACTION_COALESCE_WINDOW_MS: f64 = 2000.0;

/// Hard cap on concurrent floats (issue #1884). Beyond this a new distinct
/// reaction is DROPPED (drop-newest) so a burst can never spawn unbounded DOM /
/// animations.
pub const MAX_CONCURRENT_REACTIONS: usize = 12;

/// Lifetime of one float (ms) before its removal Timeout fires. Slightly longer
/// than the CSS rise animation so the emoji finishes its travel + fade before
/// the node is dropped. A coalesce resets the float's `born_ms` and schedules a
/// fresh timer, so a repeated reaction lives a full lifetime from its latest
/// repeat (issue #1884).
pub const REACTION_FLOAT_LIFETIME_MS: u32 = 4200;

/// Screen-reader announcement throttle (ms). At most one live-region update is
/// flushed per this interval; peer reactions arriving inside a window are
/// buffered and summarized ("{first} and {n} others reacted") so a burst can
/// never flood assistive tech with one utterance per emoji (issue #1884).
pub const REACTION_SR_THROTTLE_MS: u32 = 2000;

/// How long the reactions palette stays open after a reaction click before it
/// auto-hides (ms), so the user can fire several reactions in a row (issue
/// #1884). The timer ARMS on the first click after opening (merely opening the
/// palette does not start it) and RESTARTS on every subsequent click, throttled
/// or not. Escape / outside-click / the X close immediately and pre-empt it.
pub const REACTION_PALETTE_AUTOHIDE_MS: u32 = 5000;

/// One rising-emoji float in the overlay (issue #1884).
#[derive(Clone, PartialEq, Debug)]
pub struct FloatingReaction {
    /// Stable id (removal Timeout key + Dioxus list key).
    pub id: u64,
    /// Relay-stamped sender session id (coalesce key half; `u64::MAX` for the
    /// local "You" echo, which has no session of its own on the receive path).
    pub sender_session: u64,
    /// Rendered emoji (coalesce key half — 1:1 with the reaction enum).
    pub emoji: String,
    /// Resolved sender display name (already escaped by Dioxus at render).
    pub name: String,
    /// Repeat count; rendered as a "×N" badge when > 1.
    pub count: u32,
    /// Horizontal launch jitter in percent, in [-35.0, 35.0].
    pub offset_pct: f32,
    /// Birth time (ms). Reset on coalesce so the extended float lives a full
    /// lifetime from the latest repeat.
    pub born_ms: f64,
}

/// What [`integrate_reaction`] did with an incoming reaction, so the caller can
/// manage removal Timeouts precisely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntegrateOutcome {
    /// Coalesced into an existing float (its lifetime reset); the caller should
    /// reset that id's removal timer. Carries the existing float's id.
    Coalesced(u64),
    /// Pushed a new float; the caller schedules a removal timer for this id.
    Pushed(u64),
    /// Dropped at the concurrency cap; the caller does nothing.
    Dropped,
}

/// Index of the float `incoming` would coalesce into: the first with the same
/// `(sender_session, emoji)` born within [`REACTION_COALESCE_WINDOW_MS`], or
/// `None`. The SINGLE source of truth for the coalesce predicate — both
/// [`integrate_reaction`] (which mutates that float) and
/// [`would_integrate_mutate`] (read-only) go through it, so the "would this
/// mutate?" pre-check can never drift from the actual apply.
fn coalesce_target_index(
    active: &[FloatingReaction],
    incoming: &FloatingReaction,
    now_ms: f64,
) -> Option<usize> {
    active.iter().position(|r| {
        r.sender_session == incoming.sender_session
            && r.emoji == incoming.emoji
            && now_ms - r.born_ms < REACTION_COALESCE_WINDOW_MS
    })
}

/// Read-only: would [`integrate_reaction`] MUTATE `active` for this `incoming`?
/// A coalesce (bump an existing float) and a push (append) both mutate; a
/// drop-at-cap does NOT. Lets the caller skip a signal `write()` — and the
/// re-render it triggers — when the reaction would merely be dropped (issue
/// #1884, perf: a burst past the cap must not dirty the overlay for nothing).
pub fn would_integrate_mutate(
    active: &[FloatingReaction],
    incoming: &FloatingReaction,
    now_ms: f64,
) -> bool {
    coalesce_target_index(active, incoming, now_ms).is_some()
        || active.len() < MAX_CONCURRENT_REACTIONS
}

/// Integrate `incoming` into the `active` float list (issue #1884), mutating it
/// in place and returning the [`IntegrateOutcome`]:
///   * COALESCE — if a float with the same `(sender_session, emoji)` exists and
///     was born within [`REACTION_COALESCE_WINDOW_MS`], bump its `count` and
///     reset `born_ms` to `now_ms` (extending its lifetime); the list length is
///     unchanged.
///   * CAP — else, if `active` already holds [`MAX_CONCURRENT_REACTIONS`], DROP
///     the newcomer (drop-newest) and leave the list unchanged.
///   * PUSH — else append `incoming`.
///
/// Pure (no DOM/clock): `now_ms` is passed in, so the coalesce window is
/// deterministically host-testable. The `Dropped` arm is the ONLY non-mutating
/// outcome — [`would_integrate_mutate`] predicts it read-only.
pub fn integrate_reaction(
    active: &mut Vec<FloatingReaction>,
    incoming: FloatingReaction,
    now_ms: f64,
) -> IntegrateOutcome {
    if let Some(idx) = coalesce_target_index(active, &incoming, now_ms) {
        active[idx].count += 1;
        active[idx].born_ms = now_ms;
        return IntegrateOutcome::Coalesced(active[idx].id);
    }
    if active.len() >= MAX_CONCURRENT_REACTIONS {
        return IntegrateOutcome::Dropped;
    }
    let id = incoming.id;
    active.push(incoming);
    IntegrateOutcome::Pushed(id)
}

/// Compose the screen-reader announcement for a flushed batch of peer reactions
/// (issue #1884). `items` is `(sender_name, reaction_label)` in arrival order:
///   * empty → `None` (nothing to announce);
///   * one   → `"{name} reacted with {label}"`;
///   * many  → `"{first_name} and {n-1} others reacted"`.
///
/// The sender's OWN echo never reaches this: the relay self-skips the sender, so
/// `on_reaction` only ever fires for peers, and the local echo is pushed to the
/// overlay WITHOUT announcing.
pub fn compose_reaction_announcement(items: &[(String, String)]) -> Option<String> {
    match items.len() {
        0 => None,
        1 => Some(format!("{} reacted with {}", items[0].0, items[0].1)),
        n => Some(format!("{} and {} others reacted", items[0].0, n - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float(id: u64, sender: u64, emoji: &str, born: f64) -> FloatingReaction {
        FloatingReaction {
            id,
            sender_session: sender,
            emoji: emoji.to_string(),
            name: "n".to_string(),
            count: 1,
            offset_pct: 0.0,
            born_ms: born,
        }
    }

    // --- enum table --------------------------------------------------------

    #[test]
    fn glyph_table_complete_for_all_seven() {
        // Every defined reaction maps to a NON-EMPTY (emoji, label, slug), and
        // the slugs match the wire vocabulary / testids the spec pins.
        let expected = [
            (ReactionType::THUMBS_UP, "thumbs_up"),
            (ReactionType::THUMBS_DOWN, "thumbs_down"),
            (ReactionType::LAUGH, "laugh"),
            (ReactionType::APPLAUSE, "applause"),
            (ReactionType::HEART, "heart"),
            (ReactionType::THINKING, "thinking"),
            (ReactionType::PARTY, "party"),
        ];
        assert_eq!(REACTIONS.len(), expected.len());
        for (r, slug) in expected {
            let (emoji, label, got_slug) =
                reaction_glyph(r).expect("every 1..=7 reaction must have a glyph");
            assert!(!emoji.is_empty(), "emoji must be non-empty for {r:?}");
            assert!(!label.is_empty(), "label must be non-empty for {r:?}");
            assert_eq!(got_slug, slug, "slug mismatch for {r:?}");
        }
    }

    #[test]
    fn glyph_none_for_unspecified() {
        assert_eq!(
            reaction_glyph(ReactionType::REACTION_TYPE_UNSPECIFIED),
            None
        );
    }

    #[test]
    fn glyph_from_i32_matches_enum_path_and_rejects_unknown() {
        // 1..=7 agree with the enum-keyed table (the two paths can't drift).
        for r in REACTIONS {
            assert_eq!(reaction_glyph_from_i32(r as i32), reaction_glyph(r));
        }
        // UNSPECIFIED(0), a reserved future value (8), out-of-range (99), and a
        // negative all map to None (matches the relay allowlist).
        assert_eq!(reaction_glyph_from_i32(0), None);
        assert_eq!(reaction_glyph_from_i32(8), None);
        assert_eq!(reaction_glyph_from_i32(99), None);
        assert_eq!(reaction_glyph_from_i32(-1), None);
    }

    #[test]
    fn step_reaction_wraps_both_directions_and_recovers_from_stale() {
        // +1 advances; wraps from the last entry back to the first.
        assert_eq!(
            step_reaction(ReactionType::THUMBS_UP, 1),
            ReactionType::THUMBS_DOWN
        );
        assert_eq!(
            step_reaction(REACTIONS[REACTIONS.len() - 1], 1),
            REACTIONS[0]
        );
        // -1 retreats; wraps from the first entry to the last.
        assert_eq!(
            step_reaction(REACTIONS[0], -1),
            REACTIONS[REACTIONS.len() - 1]
        );
        // A value not in the palette resets to the first entry (not a panic /
        // out-of-range index).
        assert_eq!(
            step_reaction(ReactionType::REACTION_TYPE_UNSPECIFIED, 1),
            REACTIONS[1]
        );
    }

    // --- coalesce / cap ----------------------------------------------------

    #[test]
    fn integrate_coalesces_same_sender_and_emoji_within_window() {
        let mut active = vec![float(1, 42, "👍", 0.0)];
        // Same sender + emoji, 500ms later (< 2000ms window) → coalesce.
        let out = integrate_reaction(&mut active, float(2, 42, "👍", 500.0), 500.0);
        assert_eq!(
            out,
            IntegrateOutcome::Coalesced(1),
            "must coalesce into id 1"
        );
        assert_eq!(active.len(), 1, "no new float on coalesce");
        assert_eq!(active[0].count, 2, "count increments on coalesce");
        assert_eq!(
            active[0].born_ms, 500.0,
            "lifetime resets to now on coalesce"
        );
    }

    #[test]
    fn integrate_does_not_coalesce_across_sender_emoji_or_window() {
        // Different sender → new float.
        let mut active = vec![float(1, 42, "👍", 0.0)];
        assert!(matches!(
            integrate_reaction(&mut active, float(2, 43, "👍", 100.0), 100.0),
            IntegrateOutcome::Pushed(2)
        ));
        assert_eq!(active.len(), 2);

        // Different emoji → new float.
        let mut active = vec![float(1, 42, "👍", 0.0)];
        assert!(matches!(
            integrate_reaction(&mut active, float(2, 42, "🎉", 100.0), 100.0),
            IntegrateOutcome::Pushed(2)
        ));
        assert_eq!(active.len(), 2);

        // Same sender+emoji but OUTSIDE the window (2000ms elapsed) → new float.
        let mut active = vec![float(1, 42, "👍", 0.0)];
        assert!(matches!(
            integrate_reaction(&mut active, float(2, 42, "👍", 2000.0), 2000.0),
            IntegrateOutcome::Pushed(2)
        ));
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn integrate_drops_newest_at_the_hard_cap() {
        // Fill to MAX_CONCURRENT_REACTIONS distinct floats (distinct senders so
        // none coalesce), then the next distinct one is DROPPED.
        //
        // ADVERSARIAL: remove the cap check and the 13th would push (len 13) —
        // this test fails, which is the burst-cap guarantee the e2e also asserts.
        let mut active: Vec<FloatingReaction> = Vec::new();
        for i in 0..MAX_CONCURRENT_REACTIONS as u64 {
            let out = integrate_reaction(&mut active, float(i, i, "👍", 0.0), 0.0);
            assert!(matches!(out, IntegrateOutcome::Pushed(_)));
        }
        assert_eq!(active.len(), MAX_CONCURRENT_REACTIONS);
        let out = integrate_reaction(&mut active, float(999, 999, "👍", 0.0), 0.0);
        assert_eq!(
            out,
            IntegrateOutcome::Dropped,
            "13th distinct float must drop"
        );
        assert_eq!(
            active.len(),
            MAX_CONCURRENT_REACTIONS,
            "the list must never exceed the cap"
        );
    }

    #[test]
    fn announcement_composes_singular_plural_and_empty() {
        assert_eq!(compose_reaction_announcement(&[]), None);
        assert_eq!(
            compose_reaction_announcement(&[("Alice".into(), "thumbs up".into())]),
            Some("Alice reacted with thumbs up".to_string())
        );
        // >1 → the first sender's name + a count of the rest; the label is
        // deliberately dropped in the plural form (a batch can mix reactions).
        assert_eq!(
            compose_reaction_announcement(&[
                ("Alice".into(), "thumbs up".into()),
                ("Bob".into(), "party".into()),
                ("Cara".into(), "heart".into()),
            ]),
            Some("Alice and 2 others reacted".to_string())
        );
    }

    #[test]
    fn would_mutate_agrees_with_integrate_outcome() {
        // The write-skip optimization is only correct if would_integrate_mutate
        // is true EXACTLY when integrate_reaction would NOT return Dropped.
        // Cross-check the read-only predictor against the real apply for push,
        // coalesce, drop-at-cap, and coalesce-at-cap — if the predicate ever
        // drifts from the coalesce/cap logic this fails.
        let full: Vec<FloatingReaction> = (0..MAX_CONCURRENT_REACTIONS as u64)
            .map(|i| float(i, i, "👍", 0.0))
            .collect();
        let cases: Vec<(Vec<FloatingReaction>, FloatingReaction, f64)> = vec![
            (Vec::new(), float(1, 1, "👍", 0.0), 0.0), // push (room)
            (
                vec![float(1, 7, "👍", 0.0)],
                float(2, 7, "👍", 100.0),
                100.0,
            ), // coalesce
            (full.clone(), float(999, 999, "🎉", 0.0), 0.0), // drop at cap
            (full.clone(), float(999, 0, "👍", 100.0), 100.0), // coalesce at cap
        ];
        for (active, incoming, now) in cases {
            let predicted = would_integrate_mutate(&active, &incoming, now);
            let mut applied = active.clone();
            let outcome = integrate_reaction(&mut applied, incoming.clone(), now);
            let actually_mutated = outcome != IntegrateOutcome::Dropped;
            assert_eq!(
                predicted, actually_mutated,
                "would_integrate_mutate must agree with integrate_reaction (got outcome {outcome:?})"
            );
            // And a mutation must actually change the list; a drop must not.
            assert_eq!(actually_mutated, applied != active);
        }
    }

    #[test]
    fn integrate_coalesce_still_works_at_the_cap() {
        // At the cap, a REPEAT of an existing (sender, emoji) still coalesces
        // (it does not add a float, so the cap is not a reason to drop it).
        let mut active: Vec<FloatingReaction> = Vec::new();
        for i in 0..MAX_CONCURRENT_REACTIONS as u64 {
            integrate_reaction(&mut active, float(i, i, "👍", 0.0), 0.0);
        }
        let out = integrate_reaction(&mut active, float(999, 0, "👍", 100.0), 100.0);
        assert_eq!(out, IntegrateOutcome::Coalesced(0));
        assert_eq!(active.len(), MAX_CONCURRENT_REACTIONS);
        assert_eq!(active[0].count, 2);
    }
}
