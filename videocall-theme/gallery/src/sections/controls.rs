// SPDX-License-Identifier: MIT OR Apache-2.0

//! Buttons, icon buttons and form controls, in every variant and state.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Cell, Section, Subhead};

fn variant_name(variant: ButtonVariant) -> &'static str {
    match variant {
        ButtonVariant::Default => "Default",
        ButtonVariant::Prominent => "Prominent",
        ButtonVariant::Tinted => "Tinted",
        ButtonVariant::Plain => "Plain",
        ButtonVariant::Destructive => "Destructive",
        ButtonVariant::DestructiveProminent => "Destructive prominent",
        ButtonVariant::DestructivePlain => "Destructive plain",
        ButtonVariant::Glass => "Glass",
        ButtonVariant::GlassClear => "Glass clear",
    }
}

/// Glass variants are shown on a background they are allowed on.
fn stage_for(variant: ButtonVariant) -> &'static str {
    match variant {
        ButtonVariant::Glass => "gallery-variant-row gallery-strip gallery-busy",
        ButtonVariant::GlassClear => "gallery-variant-row gallery-strip gallery-video",
        _ => "gallery-variant-row",
    }
}

#[component]
pub fn Buttons() -> Element {
    let mut toggled = use_signal(|| false);
    rsx! {
        Section {
            id: "buttons",
            title: "Buttons",
            lede: "Hover and press are live: try them. Pressed is the toggle state (aria-pressed). The focus ring is drawn here to show it; Tab through the page to see the real one.",
            for variant in ButtonVariant::ALL {
                div { key: "{variant:?}", class: stage_for(variant),
                    span { class: "gallery-variant-name", {variant_name(variant)} }
                    div { class: "gallery-cells",
                        Cell { caption: "Default", Button { variant, "Join" } }
                        Cell { caption: "Pressed", Button { variant, pressed: true, "Join" } }
                        Cell { caption: "Disabled", Button { variant, disabled: true, "Join" } }
                        Cell { caption: "Focus ring", Button { variant, class: "gallery-focus-ring", "Join" } }
                    }
                }
            }
            Subhead { "Sizes" }
            div { class: "gallery-cells",
                for size in ButtonSize::ALL {
                    Cell { key: "{size:?}", caption: format!("{size:?}"),
                        Button { variant: ButtonVariant::Prominent, size, Icon { glyph: &icons::PLUS, "aria-hidden": "true" } "New meeting" }
                    }
                }
                Cell { caption: "Toggle",
                    Button {
                        pressed: toggled(),
                        onclick: move |_| toggled.toggle(),
                        if toggled() { "Following" } else { "Follow speaker" }
                    }
                }
            }
            Cell { caption: "Block",
                Button { variant: ButtonVariant::Prominent, size: ButtonSize::Large, block: true, "Continue" }
            }

            Subhead { "Icon buttons" }
            div { class: "gallery-cells",
                Cell { caption: "Plain", IconButton { label: "Back", Icon { glyph: &icons::CHEVRON_LEFT, "aria-hidden": "true" } } }
                Cell { caption: "Filled", IconButton { label: "Close", variant: IconButtonVariant::Filled, Icon { glyph: &icons::CLOSE, "aria-hidden": "true" } } }
                Cell { caption: "Small",
                    IconButton { label: "Delete", variant: IconButtonVariant::Filled, size: IconButtonSize::Small, Icon { glyph: &icons::TRASH, "aria-hidden": "true" } }
                }
                Cell { caption: "Pressed", IconButton { label: "Chat", pressed: true, variant: IconButtonVariant::Filled, Icon { glyph: &icons::CHAT, "aria-hidden": "true" } } }
                Cell { caption: "Disabled", IconButton { label: "Share", disabled: true, variant: IconButtonVariant::Filled, Icon { glyph: &icons::SHARE, "aria-hidden": "true" } } }
                Cell { caption: "With tooltip",
                    IconButton { label: "Participants", variant: IconButtonVariant::Filled,
                        Icon { glyph: &icons::PEOPLE, "aria-hidden": "true" }
                        Tooltip { "Participants" }
                    }
                }
            }
            div { class: "gallery-strip gallery-busy gallery-cells",
                Cell { caption: "Glass", IconButton { label: "More", variant: IconButtonVariant::Glass, Icon { glyph: &icons::MORE, "aria-hidden": "true" } } }
            }
            div { class: "gallery-strip gallery-video gallery-cells",
                Cell { caption: "Glass clear", IconButton { label: "Share", variant: IconButtonVariant::GlassClear, Icon { glyph: &icons::SHARE, "aria-hidden": "true" } } }
            }
        }
    }
}

#[component]
pub fn Forms() -> Element {
    let mut name = use_signal(|| "Dana".to_string());
    let mut search = use_signal(String::new);
    let name_error = name
        .read()
        .trim()
        .is_empty()
        .then(|| "Enter a name".to_string());
    rsx! {
        Section {
            id: "forms",
            title: "Forms",
            lede: "Inputs use 17px text so iOS does not zoom on focus. Clear the name to see the invalid state.",
            div { class: "gallery-form card",
                Field { label: "Display name", label_for: "gallery-name", hint: "Shown to everyone in the meeting", error: name_error.clone(),
                    TextField {
                        id: "gallery-name",
                        value: "{name}",
                        invalid: name_error.is_some(),
                        oninput: move |evt: FormEvent| name.set(evt.value()),
                    }
                }
                Field { label: "Meeting code", label_for: "gallery-code", hint: "Placeholder text",
                    TextField { id: "gallery-code", placeholder: "abc-defg-hij", class: "text-mono" }
                }
                Field { label: "Disabled", label_for: "gallery-disabled",
                    TextField { id: "gallery-disabled", value: "Read only", disabled: true }
                }
                Field { label: "Notes", label_for: "gallery-notes",
                    TextArea { id: "gallery-notes", placeholder: "Agenda, links, anything" }
                }
                Field { label: "Camera", label_for: "gallery-camera",
                    Select { id: "gallery-camera",
                        option { value: "front", "FaceTime HD Camera" }
                        option { value: "usb", "USB Camera" }
                        option { value: "none", "No camera" }
                    }
                }
                Field { label: "Search", label_for: "gallery-search",
                    SearchField {
                        label: "Search meetings",
                        id: "gallery-search",
                        placeholder: "Search",
                        value: "{search}",
                        oninput: move |evt: FormEvent| search.set(evt.value()),
                    }
                }
            }
        }
    }
}
