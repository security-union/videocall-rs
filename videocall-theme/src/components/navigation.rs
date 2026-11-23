// SPDX-License-Identifier: MIT OR Apache-2.0

//! Toolbar, bar, segmented control and tabs.

use dioxus::prelude::*;

use super::caller_sets;
use crate::classes::{join_classes, tab_class, with_extra};

/// A row at a screen edge (`.toolbar`). No background: the groups inside it
/// are the glass ([`Bar`]), and content scrolls under them.
///
/// `header` renders it as a `header` element, which is the page's banner
/// landmark when it is not inside an article or section.
#[component]
pub fn Toolbar(
    #[props(default)] header: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("toolbar", class.as_deref());
    if header {
        return rsx! {
            header { class, ..attributes, {children} }
        };
    }
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// The title in a toolbar (`.toolbar-title`). It truncates with an ellipsis.
#[component]
pub fn ToolbarTitle(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("toolbar-title", class.as_deref());
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// A floating glass capsule holding controls (`.bar`). `clear` switches to
/// Clear glass (`.bar-clear`), for a bar over video.
///
/// Controls inside it lose their own glass (no glass on glass), so put plain
/// [`crate::IconButton`]s or glass ones in it; both end up as fills.
///
/// `nav` renders it as a `nav` element (a navigation landmark), as for the
/// in-call control bar.
#[component]
pub fn Bar(
    #[props(default)] clear: bool,
    #[props(default)] nav: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([Some("bar"), clear.then_some("bar-clear"), class.as_deref()]);
    if nav {
        return rsx! {
            nav { class, ..attributes, {children} }
        };
    }
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// The track of a segmented control (`.segmented`), `role="group"`. Give it
/// an `aria_label`.
#[component]
pub fn Segmented(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("segmented", class.as_deref());
    rsx! {
        div { class, role: "group", ..attributes, {children} }
    }
}

/// One option of a segmented control (`.segment`). The selected one carries
/// `aria-pressed="true"`, which is what the stylesheet draws.
#[component]
pub fn Segment(
    selected: bool,
    #[props(default)] disabled: bool,
    #[props(default)] class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("segment", class.as_deref());
    rsx! {
        button {
            r#type: "button",
            class,
            disabled,
            "aria-pressed": if selected { "true" } else { "false" },
            onclick: move |evt| {
                if let Some(handler) = &onclick {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
        }
    }
}

/// A column of tabs (`.tab-list`), or a scrolling row with `horizontal`
/// (`.tab-list-horizontal`).
///
/// It is a `role="tablist"` by default. For link tabs (a settings sidebar,
/// an in-page index) pass your own `role` (`"none"` inside a `nav` is
/// usual); then neither `role="tablist"` nor `aria-orientation` is set.
#[component]
pub fn TabList(
    #[props(default)] horizontal: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([
        Some("tab-list"),
        horizontal.then_some("tab-list-horizontal"),
        class.as_deref(),
    ]);
    // Two `role` attributes on one element would leave the winner up to
    // the renderer, so ours steps aside when the caller sets one.
    let is_tablist = !caller_sets(&attributes, "role");
    let orientation = if horizontal { "horizontal" } else { "vertical" };
    rsx! {
        div {
            class,
            role: is_tablist.then_some("tablist"),
            "aria-orientation": is_tablist.then_some(orientation),
            ..attributes,
            {children}
        }
    }
}

/// One tab (`.tab`).
///
/// As a button (the default) it is `role="tab"` with `aria-selected`. With
/// `href` it is a link, marked with `.active` and `aria-current="page"`
/// when selected, which suits a settings sidebar or an in-page index.
#[component]
pub fn Tab(
    selected: bool,
    #[props(default)] href: Option<String>,
    #[props(default)] class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    if let Some(href) = href {
        let class = tab_class(selected, class.as_deref());
        return rsx! {
            a {
                class,
                href,
                "aria-current": selected.then_some("page"),
                onclick: move |evt| {
                    if let Some(handler) = &onclick {
                        handler.call(evt);
                    }
                },
                ..attributes,
                {children}
            }
        };
    }
    let class = tab_class(false, class.as_deref());
    rsx! {
        button {
            r#type: "button",
            class,
            role: "tab",
            "aria-selected": if selected { "true" } else { "false" },
            tabindex: if selected { "0" } else { "-1" },
            onclick: move |evt| {
                if let Some(handler) = &onclick {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
        }
    }
}
