// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure, host-testable UI logic for meeting reactions (issue #1884).
//!
//! Two concerns live here, both DOM/clock-free so they run under a plain native
//! `#[test]`:
//!   1. the enum→(emoji, label, slug) table — the CLIENT-side single source of
//!      truth for how a wire `ReactionType` renders (spec §1), and
//!   2. the floating-overlay integration logic — pushing EVERY reaction as its
//!      own float and enforcing the hard concurrency cap by evicting the OLDEST
//!      float (spec §4d, issue 1884 tweak: repeats each animate separately).
//!
//! The Dioxus rendering (palette + rising-emoji overlay) in `attendants.rs`
//! calls these; it does not re-implement the table or the cap math.

use videocall_client::validate_custom_emoji;
use videocall_types::protos::reaction_packet::reaction_packet::ReactionType;

/// The user-selectable quick reactions, in palette DISPLAY order: 6 positives
/// (👍😂👏❤️🤔🎉) then 5 negatives (👎😭🙅😢💔) — THUMBS_DOWN sits with the
/// negatives cluster rather than beside THUMBS_UP (issue 1884, UX). This array
/// controls ONLY rendering + roving order; the wire value is the enum
/// discriminant (unchanged) and DOM testids key on the slug, so reordering is
/// display-only. `CUSTOM` is deliberately absent — it is reached through the
/// emoji picker, not the quick row. `ReactionType` is a plain C-like enum
/// (`value()` == `*self as i32`), so callers convert to the wire i32 with
/// `reaction as i32` where needed.
pub const REACTIONS: [ReactionType; 11] = [
    // Positives.
    ReactionType::THUMBS_UP,
    ReactionType::LAUGH,
    ReactionType::APPLAUSE,
    ReactionType::HEART,
    ReactionType::THINKING,
    ReactionType::PARTY,
    // Negatives.
    ReactionType::THUMBS_DOWN,
    ReactionType::CRY,
    ReactionType::DISAGREE,
    ReactionType::SAD,
    ReactionType::HEART_BROKEN,
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

/// Client-side single source of truth: map a STATIC-glyph reaction to its
/// `(emoji, human label, dom slug)`. `REACTION_TYPE_UNSPECIFIED` and `CUSTOM`
/// map to `None`: UNSPECIFIED (and, via [`reaction_glyph_from_i32`], any
/// unknown/reserved wire value) renders nothing; `CUSTOM` has NO static glyph —
/// its emoji travels in `ReactionPacket.custom_emoji` and is rendered from that
/// string by the receive path, never from this table. The glyphs are native
/// Unicode emoji; the label is the accessible name ("React with {label}"); the
/// slug is the DOM/testid token.
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
        ReactionType::CRY => Some(("😭", "crying", "cry")),
        ReactionType::DISAGREE => Some(("🙅", "disagree", "disagree")),
        ReactionType::SAD => Some(("😢", "sad", "sad")),
        ReactionType::HEART_BROKEN => Some(("💔", "heart broken", "heart-broken")),
        // CUSTOM carries its emoji in the packet, not this table; UNSPECIFIED is
        // the non-reaction sentinel. Both render nothing from here.
        ReactionType::CUSTOM | ReactionType::REACTION_TYPE_UNSPECIFIED => None,
    }
}

/// Map a raw wire reaction value (as delivered to the `on_reaction` callback —
/// `ReactionPacket.reaction.value()`) to its STATIC glyph. `0` (UNSPECIFIED),
/// `12` (CUSTOM — rendered from `custom_emoji`, not a static glyph), and any
/// out-of-range / reserved value (13..=31, negatives, junk) map to `None`,
/// matching the relay's closed-enum allowlist. The `1..=11` arm delegates to
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
        8 => ReactionType::CRY,
        9 => ReactionType::DISAGREE,
        10 => ReactionType::SAD,
        11 => ReactionType::HEART_BROKEN,
        _ => return None,
    };
    reaction_glyph(reaction)
}

/// Hard cap on concurrent floats (issue 1884). When the overlay is full, the
/// OLDEST float is evicted to make room for a newcomer (drop-oldest), so a NEW
/// reaction always appears — a rapid burst can never be silently swallowed; at
/// worst it shortens the oldest floats' on-screen lives. Bounds DOM/animation
/// count so a burst can never spawn unbounded floats.
pub const MAX_CONCURRENT_REACTIONS: usize = 24;

/// Lifetime of one float (ms) before its removal Timeout fires. Slightly longer
/// than the CSS rise animation so the emoji finishes its travel + fade before
/// the node is dropped. Each reaction is its own float with its own timer keyed
/// on `(id, born_ms)`; a float evicted early by the drop-oldest cap simply
/// leaves its (now-stale) timer to no-op when it fires (issue 1884).
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

/// One rising-emoji float in the overlay (issue 1884). EVERY reaction — even a
/// rapid repeat of the same emoji from the same sender — is its own float with
/// its own animation; there is no coalescing/count.
#[derive(Clone, PartialEq, Debug)]
pub struct FloatingReaction {
    /// Stable id (removal Timeout key + Dioxus list key). Monotonic, so it alone
    /// uniquely identifies a float.
    pub id: u64,
    /// Relay-stamped sender session id (`u64::MAX` for the local "You" echo,
    /// which has no session of its own on the receive path). Carried for
    /// potential attribution/debug; not used to merge floats.
    pub sender_session: u64,
    /// Rendered emoji (a static-table glyph, or a validated CUSTOM emoji).
    pub emoji: String,
    /// Resolved sender display name (already escaped by Dioxus at render).
    pub name: String,
    /// Horizontal launch jitter in percent, in [-35.0, 35.0].
    pub offset_pct: f32,
    /// Birth time (ms). Half of the `(id, born_ms)` key the removal Timeout
    /// matches on so a stale timer (its float already evicted by the cap) is a
    /// no-op.
    pub born_ms: f64,
}

/// Integrate `incoming` into the `active` float list (issue 1884). EVERY
/// reaction becomes its own float — repeats of the same (sender, emoji) are NOT
/// coalesced, so each animates separately. The list is bounded at
/// [`MAX_CONCURRENT_REACTIONS`] by DROP-OLDEST: when full, the front (oldest by
/// push order) float is evicted before the newcomer is appended, so a new
/// reaction always becomes visible. Pure (no DOM/clock) so it is host-testable.
pub fn integrate_reaction(active: &mut Vec<FloatingReaction>, incoming: FloatingReaction) {
    if active.len() >= MAX_CONCURRENT_REACTIONS {
        // Evict the oldest float (front): the Vec is ordered by insertion, so
        // index 0 is the longest-lived. Its removal Timeout, still pending, will
        // find the float gone and no-op.
        active.remove(0);
    }
    active.push(incoming);
}

/// Maximum number of recently-used CUSTOM (picker) emojis kept as palette
/// quick-picks (issue 1884).
pub const MAX_RECENT_CUSTOM_EMOJIS: usize = 3;

/// Record `emoji` as the most-recently-used CUSTOM reaction in `recents` (issue
/// 1884): move-to-front with DEDUPE (an emoji already present is lifted to the
/// front, never duplicated), then cap at [`MAX_RECENT_CUSTOM_EMOJIS`], keeping
/// the list most-recent-first. The caller passes an ALREADY-validated emoji (the
/// send path validated it), so this does no allowlist check itself.
pub fn push_recent_custom_emoji(recents: &mut Vec<String>, emoji: &str) {
    recents.retain(|e| e != emoji);
    recents.insert(0, emoji.to_string());
    recents.truncate(MAX_RECENT_CUSTOM_EMOJIS);
}

/// Sanitize a persisted recents list read from `localStorage` (issue 1884): keep
/// only entries that pass the exact standard-emoji allowlist
/// ([`validate_custom_emoji`]), de-duplicated, ORDER PRESERVED (most-recent-first
/// as stored), capped at [`MAX_RECENT_CUSTOM_EMOJIS`]. A tampered storage value —
/// arbitrary text, markup, duplicates, over-length — therefore can NEVER inject a
/// non-emoji into the palette: invalid/duplicate/overflow entries are silently
/// dropped.
pub fn sanitize_recent_custom_emojis(candidates: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(MAX_RECENT_CUSTOM_EMOJIS);
    for c in candidates {
        if out.len() >= MAX_RECENT_CUSTOM_EMOJIS {
            break;
        }
        if validate_custom_emoji(&c) && !out.contains(&c) {
            out.push(c);
        }
    }
    out
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
            offset_pct: 0.0,
            born_ms: born,
        }
    }

    // --- enum table --------------------------------------------------------

    #[test]
    fn glyph_table_complete_for_all_static_reactions() {
        // Every static-glyph reaction (1..=11) maps to a NON-EMPTY
        // (emoji, label, slug), and the slugs match the wire vocabulary /
        // testids the spec pins. CUSTOM(12) + UNSPECIFIED(0) have NO static
        // glyph and are asserted separately below.
        let expected = [
            (ReactionType::THUMBS_UP, "thumbs_up"),
            (ReactionType::THUMBS_DOWN, "thumbs_down"),
            (ReactionType::LAUGH, "laugh"),
            (ReactionType::APPLAUSE, "applause"),
            (ReactionType::HEART, "heart"),
            (ReactionType::THINKING, "thinking"),
            (ReactionType::PARTY, "party"),
            (ReactionType::CRY, "cry"),
            (ReactionType::DISAGREE, "disagree"),
            (ReactionType::SAD, "sad"),
            (ReactionType::HEART_BROKEN, "heart-broken"),
        ];
        // REACTIONS (the quick palette) is exactly the 11 static reactions.
        assert_eq!(REACTIONS.len(), expected.len());
        for (r, slug) in expected {
            let (emoji, label, got_slug) =
                reaction_glyph(r).expect("every 1..=11 reaction must have a glyph");
            assert!(!emoji.is_empty(), "emoji must be non-empty for {r:?}");
            assert!(!label.is_empty(), "label must be non-empty for {r:?}");
            assert_eq!(got_slug, slug, "slug mismatch for {r:?}");
        }
    }

    #[test]
    fn glyph_none_for_unspecified_and_custom() {
        // UNSPECIFIED is the non-reaction sentinel; CUSTOM renders from the
        // packet's custom_emoji string, never from the static table. Both None.
        assert_eq!(
            reaction_glyph(ReactionType::REACTION_TYPE_UNSPECIFIED),
            None
        );
        assert_eq!(
            reaction_glyph(ReactionType::CUSTOM),
            None,
            "CUSTOM must have no static glyph — it routes through custom_emoji"
        );
    }

    #[test]
    fn every_reaction_type_value_is_handled() {
        // The Some/None contract across ALL defined enum values: the 11 static
        // reactions (== REACTIONS) have a glyph; UNSPECIFIED(0) and CUSTOM(12) do
        // not. `reaction_glyph`'s match is exhaustive, so a future enum variant
        // fails to COMPILE there until handled — this test then pins whether each
        // routes to a static glyph or to none (custom/sentinel).
        for r in REACTIONS {
            assert!(
                reaction_glyph(r).is_some(),
                "{r:?} must have a static glyph"
            );
        }
        for r in [
            ReactionType::REACTION_TYPE_UNSPECIFIED,
            ReactionType::CUSTOM,
        ] {
            assert!(
                reaction_glyph(r).is_none(),
                "{r:?} must have no static glyph"
            );
        }
    }

    #[test]
    fn glyph_from_i32_matches_enum_path_and_rejects_unknown_and_custom() {
        // 1..=11 agree with the enum-keyed table (the two paths can't drift).
        for r in REACTIONS {
            assert_eq!(reaction_glyph_from_i32(r as i32), reaction_glyph(r));
        }
        // UNSPECIFIED(0), CUSTOM(12, no static glyph), a reserved future value
        // (13), out-of-range (99), and a negative all map to None.
        assert_eq!(reaction_glyph_from_i32(0), None);
        assert_eq!(
            reaction_glyph_from_i32(12),
            None,
            "CUSTOM(12) has no static glyph — the caller renders custom_emoji"
        );
        assert_eq!(reaction_glyph_from_i32(13), None);
        assert_eq!(reaction_glyph_from_i32(99), None);
        assert_eq!(reaction_glyph_from_i32(-1), None);
    }

    #[test]
    fn step_reaction_wraps_both_directions_and_recovers_from_stale() {
        // +1 advances to the next palette entry (indices, so this stays correct
        // under a display reorder); wraps from the last entry back to the first.
        assert_eq!(step_reaction(REACTIONS[0], 1), REACTIONS[1]);
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

    // --- push-always / drop-oldest cap -------------------------------------

    #[test]
    fn integrate_pushes_every_repeat_separately() {
        // A repeat of the SAME (sender, emoji) does NOT coalesce — each becomes
        // its own float, so the list GROWS and there is no count to bump.
        //
        // ADVERSARIAL: re-introduce coalescing (merge same sender+emoji into one
        // float) and this fails — len would stay 1.
        let mut active = vec![float(1, 42, "👍", 0.0)];
        integrate_reaction(&mut active, float(2, 42, "👍", 500.0));
        integrate_reaction(&mut active, float(3, 42, "👍", 600.0));
        assert_eq!(
            active.len(),
            3,
            "three identical reactions must be three separate floats"
        );
        assert_eq!(
            active.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "each float keeps its own id, in arrival order"
        );
    }

    #[test]
    fn integrate_drops_oldest_at_the_hard_cap() {
        // Fill to MAX_CONCURRENT_REACTIONS floats, then one more: the OLDEST
        // (id 0, the front) is evicted and the newcomer appended, so the list
        // stays exactly at the cap and the newcomer is present.
        //
        // ADVERSARIAL: change drop-oldest to drop-newest (drop the incoming
        // instead of remove(0)) and the id-999 assertion fails; remove the cap
        // entirely and the length assertion fails (len would be cap+1).
        let mut active: Vec<FloatingReaction> = Vec::new();
        for i in 0..MAX_CONCURRENT_REACTIONS as u64 {
            integrate_reaction(&mut active, float(i, i, "👍", 0.0));
        }
        assert_eq!(active.len(), MAX_CONCURRENT_REACTIONS);
        assert_eq!(active[0].id, 0, "front is the oldest before eviction");

        integrate_reaction(&mut active, float(999, 999, "🎉", 1.0));
        assert_eq!(
            active.len(),
            MAX_CONCURRENT_REACTIONS,
            "the list must never exceed the cap"
        );
        assert_eq!(
            active[0].id, 1,
            "the oldest (id 0) was evicted; id 1 is now front"
        );
        assert!(
            active.iter().any(|r| r.id == 999),
            "the newest reaction must always be present (drop-oldest, not drop-newest)"
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

    // --- recent custom emojis ----------------------------------------------

    #[test]
    fn push_recent_is_most_recent_first_and_dedupes() {
        // Each push puts the emoji at the FRONT; a repeat of one already present
        // MOVES it to the front rather than duplicating.
        //
        // ADVERSARIAL: drop the `retain` (dedupe) and pushing "😭" twice would
        // leave two "😭" entries — this fails.
        let mut r: Vec<String> = Vec::new();
        push_recent_custom_emoji(&mut r, "😭");
        push_recent_custom_emoji(&mut r, "🎉");
        assert_eq!(r, vec!["🎉", "😭"], "newest is first");
        push_recent_custom_emoji(&mut r, "😭"); // re-use an existing one
        assert_eq!(
            r,
            vec!["😭", "🎉"],
            "re-using an emoji moves it to the front, no duplicate"
        );
    }

    #[test]
    fn push_recent_caps_at_three() {
        // The list never exceeds MAX_RECENT_CUSTOM_EMOJIS; the oldest falls off.
        //
        // ADVERSARIAL: remove the `truncate` and the 4th distinct push leaves 4
        // entries — this fails.
        let mut r: Vec<String> = Vec::new();
        for e in ["😀", "😁", "😂", "🤣"] {
            push_recent_custom_emoji(&mut r, e);
        }
        assert_eq!(r.len(), MAX_RECENT_CUSTOM_EMOJIS);
        assert_eq!(
            r,
            vec!["🤣", "😂", "😁"],
            "newest three kept, oldest (😀) evicted"
        );
    }

    #[test]
    fn sanitize_recents_drops_invalid_dupes_and_overflow() {
        // A tampered/legacy storage value: arbitrary text, markup, a duplicate,
        // and more than the cap. Only VALID, DISTINCT emoji survive, in order,
        // capped at three.
        //
        // ADVERSARIAL: drop the `validate_custom_emoji` guard and "hello"/
        // "<script>" would be injected into the palette — this fails (they'd
        // appear in the result).
        let raw = vec![
            "😭".to_string(),
            "hello".to_string(), // not an emoji
            "🎉".to_string(),
            "<script>".to_string(), // markup
            "😭".to_string(),       // duplicate of the first
            "🚀".to_string(),
            "❤️".to_string(), // 4th valid distinct — over the cap
        ];
        let clean = sanitize_recent_custom_emojis(raw);
        assert_eq!(
            clean,
            vec!["😭", "🎉", "🚀"],
            "only valid, distinct emoji survive, order preserved, capped at three"
        );
    }

    #[test]
    fn sanitize_recents_empty_stays_empty() {
        // No stored recents (or all invalid) yields an empty list — the palette
        // renders no recents (and no empty placeholders).
        assert!(sanitize_recent_custom_emojis(Vec::new()).is_empty());
        assert!(
            sanitize_recent_custom_emojis(vec!["nope".into(), "".into()]).is_empty(),
            "an all-invalid stored list sanitizes to empty"
        );
    }
}
