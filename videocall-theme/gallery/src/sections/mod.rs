// SPDX-License-Identifier: MIT OR Apache-2.0

//! One module per group of sections in the index.

pub mod call;
pub mod content;
pub mod controls;
pub mod foundations;
pub mod materials;
pub mod navigation;
pub mod overlays;

use dioxus::prelude::*;

/// A linkable section: `id` is the hash anchor, `title` its heading.
#[component]
pub fn Section(
    id: &'static str,
    title: &'static str,
    lede: &'static str,
    children: Element,
) -> Element {
    rsx! {
        section { id, class: "gallery-section", aria_labelledby: "{id}-title",
            h2 { id: "{id}-title", class: "gallery-section-title", "{title}" }
            p { class: "gallery-lede", "{lede}" }
            {children}
        }
    }
}

/// A sub-heading inside a section.
#[component]
pub fn Subhead(children: Element) -> Element {
    rsx! {
        h3 { class: "gallery-subhead", {children} }
    }
}

/// One labelled sample in a row of samples.
#[component]
pub fn Cell(caption: String, children: Element) -> Element {
    rsx! {
        figure { class: "gallery-cell",
            div { class: "gallery-cell-sample", {children} }
            figcaption { class: "gallery-cell-caption", "{caption}" }
        }
    }
}
