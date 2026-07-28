// SPDX-License-Identifier: MIT OR Apache-2.0

//! The standard-emoji picker panel for the CUSTOM reaction (issue 1884),
//! extracted into its own child component so it forms an isolated reactive
//! scope.
//!
//! PERF (issue 1884 perf review): the category grid can hold ~388 buttons (e.g.
//! People & Body). Reading `emoji_group` HERE — not inline in the ~9,000-line
//! `AttendantsComponent` — means an UNRELATED attendants re-render (speaking
//! indicators, stats ticks, peer churn) while the picker sits open no longer
//! re-pays the ~388-VNode + ~776-String grid rebuild: this child is memoized on
//! its props (the `emoji_group` signal handle + the send handler), so it
//! re-renders ONLY when the selected category actually changes. Mirrors the
//! `ReactionsOverlay` isolation.

use dioxus::prelude::*;
use videocall_client::validate_custom_emoji;

/// Stable DOM/testid slug + human label for an emoji-picker category (issue
/// 1884). The slug is the `emoji-group-{slug}` testid token; the label is the
/// accessible tab name. Exhaustive over `emojis::Group`, so a future crate
/// bump that adds a group fails to compile here rather than shipping an
/// unlabeled tab.
pub fn emoji_group_meta(group: emojis::Group) -> (&'static str, &'static str) {
    use emojis::Group::*;
    match group {
        SmileysAndEmotion => ("smileys-and-emotion", "Smileys & Emotion"),
        PeopleAndBody => ("people-and-body", "People & Body"),
        AnimalsAndNature => ("animals-and-nature", "Animals & Nature"),
        FoodAndDrink => ("food-and-drink", "Food & Drink"),
        TravelAndPlaces => ("travel-and-places", "Travel & Places"),
        Activities => ("activities", "Activities"),
        Objects => ("objects", "Objects"),
        Symbols => ("symbols", "Symbols"),
        Flags => ("flags", "Flags"),
    }
}

/// The standard-emoji picker panel (CUSTOM reaction, issue 1884): category
/// toggles + a scrollable grid for the SELECTED category only, so the full
/// ~3800-emoji table is never mounted at once. `emoji_group` is read + written
/// here (tab click), and each grid button calls `send_custom_reaction` with its
/// glyph. Rendered inside `.reactions-palette` by `AttendantsComponent` only
/// while the picker is open; Arrow/Home/End keydowns are stopped here so the
/// palette's roving handler does not yank focus back to the quick row (Escape
/// and Tab still bubble/work). The recents quick-picks stay in the palette (the
/// parent), not here.
#[component]
pub fn EmojiPicker(
    mut emoji_group: Signal<emojis::Group>,
    send_custom_reaction: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            class: "emoji-picker",
            "data-testid": "emoji-picker",
            role: "group",
            "aria-label": "Choose an emoji",
            onkeydown: move |evt: Event<KeyboardData>| {
                let key = evt.key();
                if key == Key::ArrowRight
                    || key == Key::ArrowLeft
                    || key == Key::ArrowUp
                    || key == Key::ArrowDown
                    || key == Key::Home
                    || key == Key::End
                {
                    evt.stop_propagation();
                }
            },
            // Category switcher: a group of toggle buttons (one representative
            // glyph per group). Deliberately NOT role=tab/tablist — a full ARIA
            // tabs widget also needs a tabpanel + arrow-key roving, which we do
            // not implement; a group of `aria-pressed` toggles is the honest,
            // non-misleading contract.
            div {
                class: "emoji-picker__tabs",
                role: "group",
                "aria-label": "Emoji categories",
                for group in emojis::Group::iter() {
                    {
                        let (slug, label) = emoji_group_meta(group);
                        let selected = emoji_group() == group;
                        let tab_glyph = group.emojis().next().map(|e| e.as_str()).unwrap_or("");
                        rsx! {
                            button {
                                key: "{slug}",
                                class: if selected { "emoji-tab active" } else { "emoji-tab" },
                                r#type: "button",
                                "aria-pressed": if selected { "true" } else { "false" },
                                "data-testid": "emoji-group-{slug}",
                                "aria-label": "{label}",
                                title: "{label}",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    emoji_group.set(group);
                                },
                                span { class: "reaction-option__emoji", "aria-hidden": "true", "{tab_glyph}" }
                            }
                        }
                    }
                }
            }
            // Scrollable grid for the selected category. Emoji over the byte cap
            // are filtered out (via validate_custom_emoji) so every shown option
            // is sendable. testid index is 0-based within THIS filtered category
            // grid.
            div {
                class: "emoji-picker__grid",
                "data-testid": "emoji-picker-grid",
                for (i, emoji) in emoji_group()
                    .emojis()
                    .filter(|e| validate_custom_emoji(e.as_str()))
                    .enumerate()
                {
                    {
                        let glyph = emoji.as_str().to_string();
                        let name = emoji.name().to_string();
                        rsx! {
                            button {
                                key: "{glyph}",
                                class: "reaction-option emoji-option",
                                r#type: "button",
                                "data-testid": "emoji-option-{i}",
                                "aria-label": "React with {name}",
                                title: "{name}",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    send_custom_reaction.call(glyph.clone());
                                },
                                span { class: "reaction-option__emoji", "aria-hidden": "true", "{glyph}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
