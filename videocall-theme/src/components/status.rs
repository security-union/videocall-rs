// SPDX-License-Identifier: MIT OR Apache-2.0

//! Badge, status dot, spinner, and the material surfaces.

use dioxus::prelude::*;

use crate::classes::{join_classes, BadgeTone, DimEdge, Material, StatusTone};

/// A small label pill (`.badge`). The tone tints the fill; the text stays
/// the label color, so it reads on every background.
#[component]
pub fn Badge(
    #[props(default)] tone: BadgeTone,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([Some("badge"), tone.class(), class.as_deref()]);
    rsx! {
        span { class, ..attributes, {children} }
    }
}

/// An 8px dot (`.status-dot`).
///
/// With `label` it is `role="img"` with that `aria-label`. Without one it is
/// hidden from screen readers, so put the meaning in visible text next to it.
#[component]
pub fn StatusDot(
    #[props(default)] tone: StatusTone,
    #[props(default)] label: Option<String>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let class = join_classes([Some("status-dot"), tone.class(), class.as_deref()]);
    let hidden = label.is_none().then_some("true");
    let role = label.is_some().then_some("img");
    rsx! {
        span {
            class,
            role,
            "aria-label": label,
            "aria-hidden": hidden,
            ..attributes,
        }
    }
}

/// A spinner (`.spinner`) in the current text color. `large` is
/// `.spinner-lg`.
///
/// With a `label` (the default is "Loading") it is `role="status"` with that
/// `aria-label`. `label: None` renders the bare spinner with no role and no
/// name, for one that sits next to text saying the same thing ("Loading
/// meeting...", a "Deleting..." button).
#[component]
pub fn Spinner(
    #[props(default)] large: bool,
    /// Accessible name. Defaults to "Loading"; `None` for none.
    #[props(default = Some("Loading".to_string()))]
    label: Option<String>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let class = join_classes([
        Some("spinner"),
        large.then_some("spinner-lg"),
        class.as_deref(),
    ]);
    let role = label.is_some().then_some("status");
    rsx! {
        span { class, role, "aria-label": label, ..attributes }
    }
}

/// A glass surface (`.glass-regular` or `.glass-clear`) for a custom
/// floating panel such as a sidebar. Bars, sheets, menus and toasts bring
/// their own glass; do not wrap them in this.
#[component]
pub fn Glass(
    #[props(default)] material: Material,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([Some(material.class()), class.as_deref()]);
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// The 35% dimming layer for Clear glass over bright media (`.media-dim`,
/// `.media-dim-top`, `.media-dim-bottom`). Absolute, inset 0: put it inside
/// the positioned media container, after the video. It is an empty `div`
/// with no role, which assistive tech skips already, so it carries no ARIA.
#[component]
pub fn MediaDim(
    #[props(default)] edge: DimEdge,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let class = join_classes([Some(edge.class()), class.as_deref()]);
    rsx! {
        div { class, ..attributes }
    }
}
