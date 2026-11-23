// SPDX-License-Identifier: MIT OR Apache-2.0

//! Toolbar, bar, segmented control, tabs, and the status primitives.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Cell, Section, Subhead};

#[component]
pub fn Navigation() -> Element {
    let mut layout = use_signal(|| 0usize);
    let mut tab = use_signal(|| 0usize);
    let layouts = ["Grid", "Speaker", "Sidebar"];
    let tabs = ["General", "Audio", "Video", "Network"];
    rsx! {
        Section {
            id: "navigation",
            title: "Navigation",
            lede: "A toolbar is only a row; the bars inside it are the glass, and content scrolls under them.",
            Subhead { "Toolbar with bars" }
            div { class: "gallery-demo gallery-busy gallery-demo-flush",
                Toolbar {
                    Bar {
                        IconButton { label: "Back", Icon { glyph: &icons::CHEVRON_LEFT, "aria-hidden": "true" } }
                    }
                    ToolbarTitle { class: "gallery-toolbar-title", "Weekly sync with a long title that truncates" }
                    Bar {
                        IconButton { label: "Participants", Icon { glyph: &icons::PEOPLE, "aria-hidden": "true" } }
                        IconButton { label: "Chat", Icon { glyph: &icons::CHAT, "aria-hidden": "true" } }
                    }
                }
            }
            Subhead { "Segmented" }
            div { class: "gallery-cells",
                Cell { caption: "Selected by aria-pressed",
                    Segmented { aria_label: "Layout",
                        for (index, label) in layouts.into_iter().enumerate() {
                            Segment { key: "{label}", selected: layout() == index, onclick: move |_| layout.set(index), "{label}" }
                        }
                    }
                }
                Cell { caption: "With a disabled option",
                    Segmented { aria_label: "Quality",
                        Segment { selected: true, "Auto" }
                        Segment { selected: false, "HD" }
                        Segment { selected: false, disabled: true, "4K" }
                    }
                }
            }
            Subhead { "Tabs" }
            div { class: "gallery-columns",
                Card {
                    TabList { aria_label: "Settings",
                        for (index, label) in tabs.into_iter().enumerate() {
                            Tab { key: "{label}", selected: tab() == index, onclick: move |_| tab.set(index), "{label}" }
                        }
                    }
                }
                Card {
                    TabList { horizontal: true, aria_label: "Settings, as a row",
                        for (index, label) in tabs.into_iter().enumerate() {
                            Tab { key: "{label}", selected: tab() == index, onclick: move |_| tab.set(index), "{label}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Status() -> Element {
    let tones = [
        (BadgeTone::Neutral, "Neutral"),
        (BadgeTone::Accent, "Host"),
        (BadgeTone::Success, "Connected"),
        (BadgeTone::Warning, "Weak signal"),
        (BadgeTone::Danger, "Recording"),
    ];
    let dots = [
        (StatusTone::Neutral, "Idle"),
        (StatusTone::Accent, "Speaking"),
        (StatusTone::Success, "Live"),
        (StatusTone::Warning, "Reconnecting"),
        (StatusTone::Danger, "Offline"),
    ];
    rsx! {
        Section {
            id: "status",
            title: "Status",
            lede: "Badge tones tint the fill and keep the label color, because green or orange text on a light page is far under 4.5:1.",
            Subhead { "Badges" }
            div { class: "row gallery-wrap",
                for (tone, label) in tones {
                    Badge { key: "{label}", tone, "{label}" }
                }
                Badge { tone: BadgeTone::Count, "3" }
                Badge { tone: BadgeTone::Count, "99+" }
            }
            Subhead { "Status dots" }
            div { class: "row gallery-wrap gap-4",
                for (tone, label) in dots {
                    span { key: "{label}", class: "row gap-2 text-subheadline",
                        StatusDot { tone }
                        "{label}"
                    }
                }
            }
            Subhead { "Spinners" }
            div { class: "gallery-cells",
                Cell { caption: "Regular", Spinner {} }
                Cell { caption: "Large", Spinner { large: true } }
                Cell { caption: "In a button",
                    Button { disabled: true, Spinner { label: "Joining" } "Joining" }
                }
                Cell { caption: "Accent", span { class: "gallery-accent-text", Spinner { large: true } } }
            }
        }
    }
}
