// SPDX-License-Identifier: MIT OR Apache-2.0

//! The floating reactions overlay + screen-reader live region (issue #1884),
//! extracted into its own component so it forms a small, isolated reactive
//! scope.
//!
//! PERF (#1884 perf review): `active_reactions` / `reaction_announcement` are
//! read HERE, not inline in `AttendantsComponent`. Reading a signal subscribes
//! the reading component, so keeping these reads in this ~15-node child means an
//! incoming reaction re-renders only this overlay — NOT the ~8,900-line
//! attendants RSX and every keyed `PeerTile` (the #1296 blast-radius hazard).
//! The parent only PASSES the signal handles (a copy, which does not
//! subscribe); the send/receive handlers WRITE them (also no subscription).

use crate::components::reactions::FloatingReaction;
use dioxus::prelude::*;

/// Renders the live rising-emoji floats and the polite SR live region. Both
/// signals are read-only here (the floats are pushed/removed, and the
/// announcement is flushed, by the handlers/timers that own the writes in
/// `AttendantsComponent`).
#[component]
pub fn ReactionsOverlay(
    active_reactions: ReadSignal<Vec<FloatingReaction>>,
    reaction_announcement: ReadSignal<String>,
) -> Element {
    rsx! {
        // Fixed, bottom-centre lane of rising emoji (local echo + peers),
        // independent of dock position. aria-hidden so the visual channel never
        // double-announces; the sole SR channel is the live region below.
        // pointer-events:none everywhere (CSS) so it never eats clicks meant for
        // the tiles / action bar beneath it.
        div {
            class: "reactions-overlay",
            "data-testid": "reactions-overlay",
            "aria-hidden": "true",
            for float in active_reactions() {
                div {
                    key: "{float.id}",
                    class: "reaction-float",
                    "data-testid": "reaction-float",
                    // UX NB-1: anchor on the overlay CENTRE, not its left edge —
                    // absolute children ignore the overlay's flex centering, so a
                    // bare `left: {offset}%` clusters floats at the left and clips
                    // negative offsets. `calc(50% + offset%)` spreads them around
                    // centre; the reaction-rise keyframes carry the matching
                    // translateX(-50%) so the float is centred on that point.
                    style: "left: calc(50% + {float.offset_pct}%)",
                    span { class: "reaction-float__emoji", "{float.emoji}" }
                    if float.count > 1 {
                        span {
                            class: "reaction-float__count",
                            "data-testid": "reaction-float-count",
                            "×{float.count}"
                        }
                    }
                    span {
                        class: "reaction-float__name",
                        "data-testid": "reaction-float-name",
                        "{float.name}"
                    }
                }
            }
        }

        // Screen-reader live region. Visually hidden; role=status + polite +
        // atomic so each throttled flush is announced as ONE utterance. This is
        // the only SR channel (the overlay above is aria-hidden); the sender's
        // own echo is never announced (see the on_reaction callback).
        div {
            class: "visually-hidden",
            "data-testid": "reaction-live-region",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "{reaction_announcement}"
        }
    }
}
