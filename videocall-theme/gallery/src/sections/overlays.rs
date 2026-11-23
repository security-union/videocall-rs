// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sheet, alert, menu, toast and tooltip. The open/close logic lives here,
//! in the gallery, because the primitives only draw.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Cell, Section, Subhead};
use crate::icons::{self, Icon};
use crate::Gallery;

#[derive(Clone, Copy, PartialEq)]
enum Open {
    None,
    Sheet,
    WideSheet,
    Alert,
}

/// Moves focus into a dialog when it opens, so Escape reaches the scrim's
/// key handler and the next Tab stays inside.
async fn focus(evt: MountedEvent) {
    let _ = evt.set_focus(true).await;
}

#[component]
pub fn Overlays() -> Element {
    let mut gallery = use_context::<Gallery>();
    let mut open = use_signal(|| Open::None);
    let mut menu_open = use_signal(|| true);
    let close = move |_| open.set(Open::None);
    let on_key = move |evt: KeyboardEvent| {
        if evt.key() == Key::Escape {
            open.set(Open::None);
        }
    };

    rsx! {
        Section {
            id: "overlays",
            title: "Overlays",
            lede: "The scrim dims without blur, because blurring the whole screen over live video costs too much. Sheets drop to the bottom on narrow screens; alerts stay centered.",
            div { class: "row gallery-wrap",
                Button { onclick: move |_| open.set(Open::Sheet), "Open sheet" }
                Button { onclick: move |_| open.set(Open::WideSheet), "Open wide sheet" }
                Button { variant: ButtonVariant::Destructive, onclick: move |_| open.set(Open::Alert), "Open alert" }
                Button { variant: ButtonVariant::Tinted, onclick: move |_| gallery.show_toast("Link copied"), "Show a toast" }
            }

            Subhead { "Menu" }
            div { class: "gallery-demo gallery-busy",
                div { class: "gallery-menu-anchor",
                    IconButton {
                        label: "Meeting options",
                        variant: IconButtonVariant::Glass,
                        pressed: menu_open(),
                        aria_expanded: if menu_open() { "true" } else { "false" },
                        onclick: move |_| menu_open.toggle(),
                        Icon { paths: icons::MORE }
                    }
                    if menu_open() {
                        Menu { aria_label: "Meeting options",
                            MenuHeader { "Weekly sync" }
                            MenuItem { onclick: move |_| gallery.show_toast("Link copied"), Icon { paths: icons::SHARE } "Copy invite link" }
                            MenuItem { Icon { paths: icons::PEOPLE } "Participants" }
                            MenuItem { disabled: true, Icon { paths: icons::SCREEN } "Share screen" }
                            MenuSeparator {}
                            MenuItem { destructive: true, Icon { paths: icons::TRASH } "End for everyone" }
                        }
                    }
                }
            }

            Subhead { "Toast" }
            div { class: "gallery-cells",
                Cell { caption: "Regular glass (.toast)",
                    div { class: "gallery-demo",
                        Toast { span { class: "grow", "Dana joined the meeting" } Button { variant: ButtonVariant::Plain, size: ButtonSize::Small, "View" } }
                    }
                }
                Cell { caption: "Over video (.toast-solid)",
                    div { class: "theme-dark gallery-demo gallery-strip gallery-video",
                        Toast { solid: true, span { class: "grow", "Dana joined the meeting" } }
                    }
                }
            }

            Subhead { "Tooltip" }
            div { class: "gallery-cells gallery-tooltip-row",
                Cell { caption: "Above, on hover or focus",
                    IconButton { label: "Chat", variant: IconButtonVariant::Filled, Icon { paths: icons::CHAT } Tooltip { "Chat" } }
                }
                Cell { caption: "Below",
                    IconButton { label: "Settings", variant: IconButtonVariant::Filled, Icon { paths: icons::SETTINGS } Tooltip { placement: TooltipPlacement::Below, "Settings" } }
                }
                Cell { caption: "Pinned open (.is-open)",
                    IconButton { label: "Participants", variant: IconButtonVariant::Filled, Icon { paths: icons::PEOPLE } Tooltip { open: true, "Participants" } }
                }
            }
            p { class: "text-subheadline text-secondary",
                "At a screen edge a centered tooltip hangs over the edge. .tooltip-start and .tooltip-end line it up with the control instead, "
                "and a long one wraps before it gets wider than the screen."
            }
            div { class: "gallery-tooltip-edges",
                IconButton { label: "Microphone", variant: IconButtonVariant::Filled, Icon { paths: icons::MIC }
                    Tooltip { align: TooltipAlign::Start, open: true, "Start (.tooltip-start)" }
                }
                IconButton { label: "Leave", variant: IconButtonVariant::Filled, Icon { paths: icons::HANG_UP }
                    Tooltip { align: TooltipAlign::End, open: true, "End (.tooltip-end)" }
                }
            }
        }

        match open() {
            Open::None => rsx! {},
            Open::Sheet | Open::WideSheet => rsx! {
                Scrim { onclick: close, onkeydown: on_key,
                    Sheet { wide: open() == Open::WideSheet, grabber: true, aria_labelledby: "gallery-sheet-title",
                        div { class: "gallery-focus-target", tabindex: "-1", onmounted: focus }
                        SheetHeader {
                            SheetTitle { id: "gallery-sheet-title", if open() == Open::WideSheet { "Settings" } else { "Devices" } }
                            IconButton { label: "Close", variant: IconButtonVariant::Filled, size: IconButtonSize::Small, onclick: close, Icon { paths: icons::CLOSE } }
                        }
                        SheetBody {
                            div { class: "stack gap-4",
                                Field { label: "Camera", label_for: "gallery-sheet-camera",
                                    Select { id: "gallery-sheet-camera",
                                        option { "FaceTime HD Camera" }
                                        option { "USB Camera" }
                                    }
                                }
                                ListGroup {
                                    ListRow { ListRowLabel { "Mirror my video" } ListRowValue { "On" } }
                                    ListRow { ListRowLabel { "HD" } ListRowValue { "Auto" } }
                                }
                            }
                        }
                        SheetFooter {
                            Button { onclick: close, "Cancel" }
                            Button { variant: ButtonVariant::Prominent, onclick: close, "Save" }
                        }
                    }
                }
            },
            Open::Alert => rsx! {
                Scrim { onclick: close, onkeydown: on_key,
                    Alert { aria_labelledby: "gallery-alert-title", aria_describedby: "gallery-alert-message",
                        div { class: "gallery-focus-target", tabindex: "-1", onmounted: focus }
                        AlertTitle { id: "gallery-alert-title", "End the meeting for everyone?" }
                        AlertMessage { id: "gallery-alert-message", "Everyone will be disconnected. You can start it again later." }
                        AlertActions {
                            Button { onclick: close, "Cancel" }
                            Button { variant: ButtonVariant::DestructiveProminent, onclick: close, "End" }
                        }
                    }
                }
            },
        }
    }
}
