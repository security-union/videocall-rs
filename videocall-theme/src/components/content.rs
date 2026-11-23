// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cards and grouped lists. Content layer, never glass.

use dioxus::prelude::*;

use crate::classes::with_extra;

/// A rounded content container (`.card`). Put it on a grouped background.
#[component]
pub fn Card(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("card", class.as_deref());
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// The title line of a card (`h3.card-title`).
#[component]
pub fn CardTitle(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("card-title", class.as_deref());
    rsx! {
        h3 { class, ..attributes, {children} }
    }
}

/// A rounded group of rows (`.list-group`).
///
/// `list` renders it as a `ul`, so a screen reader announces a list and
/// counts its items. Its rows are then `ListRow { list_item: true }`.
#[component]
pub fn ListGroup(
    #[props(default)] list: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-group", class.as_deref());
    if list {
        return rsx! {
            ul { class, ..attributes, {children} }
        };
    }
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// One row of a list group (`.list-row`).
///
/// It renders as a link with `href`, as a button with `onclick`, and as a
/// plain `div` otherwise (an `li` with `list_item`, inside
/// `ListGroup { list: true }`). Only the link and button get hover and press
/// states, so a row holding a switch stays a `div` and the switch is the
/// control. `list_item` applies to the plain row only.
#[component]
pub fn ListRow(
    #[props(default)] href: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(default)] disabled: bool,
    #[props(default)] list_item: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-row", class.as_deref());
    if let Some(href) = href {
        return rsx! {
            a { class, href, ..attributes, {children} }
        };
    }
    if let Some(handler) = onclick {
        return rsx! {
            button {
                r#type: "button",
                class,
                disabled,
                onclick: move |evt| handler.call(evt),
                ..attributes,
                {children}
            }
        };
    }
    if list_item {
        return rsx! {
            li { class, ..attributes, {children} }
        };
    }
    rsx! {
        div { class, ..attributes, {children} }
    }
}

/// The leading text of a row (`.list-row-label`). It takes the free width.
/// `subtitle` adds a second, smaller line.
#[component]
pub fn ListRowLabel(
    #[props(default)] subtitle: Option<String>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-row-label", class.as_deref());
    rsx! {
        span { class, ..attributes,
            {children}
            if let Some(subtitle) = subtitle {
                span { class: "list-row-subtitle", "{subtitle}" }
            }
        }
    }
}

/// The trailing value or control of a row (`.list-row-value`).
#[component]
pub fn ListRowValue(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-row-value", class.as_deref());
    rsx! {
        span { class, ..attributes, {children} }
    }
}

/// The section header above a group (`h3.list-header`). Normal case.
#[component]
pub fn ListHeader(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-header", class.as_deref());
    rsx! {
        h3 { class, ..attributes, {children} }
    }
}

/// The note below a group (`p.list-footer`).
#[component]
pub fn ListFooter(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("list-footer", class.as_deref());
    rsx! {
        p { class, ..attributes, {children} }
    }
}
