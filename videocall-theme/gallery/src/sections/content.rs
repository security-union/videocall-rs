// SPDX-License-Identifier: MIT OR Apache-2.0

//! A grouped settings list and a card, the way a settings screen uses them.

use dioxus::prelude::*;
use videocall_theme::*;

use super::Section;
use crate::icons::{self, Icon};
use crate::Gallery;

#[component]
pub fn Lists() -> Element {
    let mut gallery = use_context::<Gallery>();
    let mut noise = use_signal(|| true);
    let mut echo = use_signal(|| false);
    rsx! {
        Section {
            id: "lists",
            title: "Lists and cards",
            lede: "Content, never glass. Groups sit on the grouped background, rows are at least 44px with inset separators, and only button or link rows react to hover and press.",
            div { class: "gallery-columns",
                div { class: "stack gap-1",
                    ListHeader { "Audio" }
                    ListGroup {
                        ListRow {
                            ListRowLabel { id: "gallery-noise", "Noise suppression" }
                            ListRowValue {
                                Switch { enabled: noise(), on_toggle: move |on| noise.set(on), labelled_by: "gallery-noise", test_id: "noise-toggle" }
                            }
                        }
                        ListRow {
                            ListRowLabel { id: "gallery-echo", subtitle: "Helps on laptop speakers", "Echo cancellation" }
                            ListRowValue {
                                Switch { enabled: echo(), on_toggle: move |on| echo.set(on), labelled_by: "gallery-echo" }
                            }
                        }
                        ListRow {
                            ListRowLabel { id: "gallery-gain", subtitle: "Managed by this device", "Automatic gain" }
                            ListRowValue {
                                Switch { enabled: true, disabled: true, on_toggle: |_| {}, labelled_by: "gallery-gain" }
                            }
                        }
                    }
                    ListFooter { "Changes apply to the next call." }
                }
                div { class: "stack gap-1",
                    ListHeader { "Video" }
                    ListGroup {
                        ListRow {
                            ListRowLabel { "Resolution" }
                            ListRowValue { "720p" }
                        }
                        ListRow { onclick: move |_| gallery.show_toast("Opened encoder settings"),
                            ListRowLabel { "Encoder settings" }
                            ListRowValue { Icon { paths: icons::CHEVRON_RIGHT } }
                        }
                        ListRow { href: "#lists",
                            ListRowLabel { subtitle: "A link row", "Diagnostics" }
                            ListRowValue { Badge { tone: BadgeTone::Success, "Good" } }
                        }
                    }
                }
                Card { class: "stack gap-2",
                    CardTitle { "Weekly sync" }
                    p { class: "text-subheadline text-secondary", "Tuesdays at 10:00. 6 people invited." }
                    div { class: "row gallery-wrap",
                        Button { variant: ButtonVariant::Prominent, size: ButtonSize::Small, "Join" }
                        Button { size: ButtonSize::Small, "Copy link" }
                    }
                }
            }
        }
    }
}
