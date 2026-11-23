// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regular and Clear glass, the dimming layer, the no-glass-on-glass rule
//! and the `.over-media` scope, over a busy background and over a stand-in
//! for video.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Section, Subhead};

/// A loud background for glass to sit on: stripes and big type.
#[component]
pub fn BusyBackdrop() -> Element {
    rsx! {
        div { class: "gallery-busy-text", "aria-hidden": "true",
            "Content scrolls under the glass. Content scrolls under the glass."
        }
    }
}

#[component]
pub fn Materials() -> Element {
    let mut dim = use_signal(|| true);
    rsx! {
        Section {
            id: "materials",
            title: "Materials",
            lede: "Glass is only for the layer that floats above content. Blur and tint are approximations; Apple publishes no number. The 35% dim under Clear glass is Apple's.",
            Subhead { "Regular glass over content" }
            div { class: "gallery-stage gallery-busy",
                BusyBackdrop {}
                Glass { class: "gallery-panel",
                    div { class: "text-headline", "Regular" }
                    p { class: "text-subheadline", "Frosty enough to read on its own, since Safari cannot report Reduce Transparency." }
                }
                div { class: "stack gap-2",
                    span { class: "gallery-stage-caption", "On content: glass" }
                    div { class: "row gallery-wrap",
                        Button { variant: ButtonVariant::Glass, "Glass button" }
                        IconButton { label: "More", variant: IconButtonVariant::Glass, Icon { glyph: &icons::MORE, "aria-hidden": "true" } }
                    }
                    span { class: "gallery-stage-caption", "Inside a bar: the same controls turn into fills" }
                    Bar {
                        Button { variant: ButtonVariant::Glass, "Glass button" }
                        IconButton { label: "More", variant: IconButtonVariant::Glass, Icon { glyph: &icons::MORE, "aria-hidden": "true" } }
                    }
                }
            }

            Subhead { "Clear glass over video" }
            div { class: "gallery-stage gallery-video",
                if dim() {
                    MediaDim {}
                }
                Glass { material: Material::Clear, class: "gallery-panel",
                    div { class: "text-headline", "Clear" }
                    p { class: "text-subheadline", "Only over video or photos. Bright pictures need the dimming layer under it." }
                }
                div { class: "stack gap-2",
                    Bar { clear: true,
                        ControlButton { label: "Microphone", state: ControlState::On, Icon { glyph: &icons::MIC, "aria-hidden": "true" } }
                        ControlButton { label: "Camera", Icon { glyph: &icons::CAMERA_OFF, "aria-hidden": "true" } }
                        Button { variant: ButtonVariant::GlassClear, size: ButtonSize::Small, "Clear button" }
                    }
                    div { class: "row gallery-wrap",
                        Button { variant: ButtonVariant::GlassClear, "On video" }
                        IconButton { label: "Share", variant: IconButtonVariant::GlassClear, Icon { glyph: &icons::SHARE, "aria-hidden": "true" } }
                    }
                }
            }
            Subhead { "Over live media: the .over-media scope" }
            div { class: "gallery-stage gallery-video over-media",
                if dim() {
                    MediaDim {}
                }
                Glass { class: "gallery-panel",
                    div { class: "text-headline", "Regular, inside .over-media" }
                    p { class: "text-subheadline", "A screen with live video puts .over-media on its root. Regular turns into an opaque elevated fill with no blur, so Clear is the only glass and nothing big re-blurs the video every frame." }
                }
                div { class: "stack gap-2",
                    Bar { clear: true,
                        ControlButton { label: "Microphone", state: ControlState::On, Icon { glyph: &icons::MIC, "aria-hidden": "true" } }
                        ControlButton { label: "Camera", Icon { glyph: &icons::CAMERA_OFF, "aria-hidden": "true" } }
                    }
                    span { class: "gallery-stage-caption", "Clear glass is unchanged" }
                }
            }
            ListGroup {
                ListRow {
                    ListRowLabel { id: "gallery-dim-label", subtitle: "35% black, Apple's number. Use .media-dim-top or -bottom for a bar at one edge.",
                        "Dim the video"
                    }
                    ListRowValue {
                        Switch {
                            enabled: dim(),
                            on_toggle: move |on| dim.set(on),
                            labelled_by: "gallery-dim-label",
                        }
                    }
                }
            }
            p { class: "text-footnote text-secondary",
                "Without backdrop-filter, under Reduce Transparency and under Increase Contrast, glass turns near-opaque with a visible border. "
                "Try the last two from the bar at the bottom."
            }
        }
    }
}
