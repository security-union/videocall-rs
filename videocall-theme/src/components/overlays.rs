// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scrim, sheet, alert, menu, toast and tooltip.
//!
//! These only draw. Opening, closing, focus and timers belong to the caller.
//! One small behavior is built in: a click inside a [`Sheet`] or [`Alert`]
//! does not reach the [`Scrim`] around it, so `Scrim { onclick }` fires only
//! for clicks on the backdrop.

use dioxus::prelude::*;

use super::caller_sets;
use crate::classes::{
    join_classes, toast_class, tooltip_class, with_extra, TooltipAlign, TooltipPlacement,
};

/// Full-screen dimmer that centers its child (`.scrim`). On narrow screens
/// the child sits at the bottom.
#[component]
pub fn Scrim(
    #[props(default)] class: Option<String>,
    /// Backdrop clicks. Clicks inside a `Sheet` or `Alert` never get here.
    #[props(default)]
    onclick: Option<EventHandler<MouseEvent>>,
    #[props(default)] onkeydown: Option<EventHandler<KeyboardEvent>>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("scrim", class.as_deref());
    rsx! {
        div {
            class,
            onclick: move |evt| {
                if let Some(handler) = &onclick {
                    handler.call(evt);
                }
            },
            onkeydown: move |evt| {
                if let Some(handler) = &onkeydown {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
        }
    }
}

/// A dialog on wide screens, an inset bottom sheet on narrow ones
/// (`.sheet`). Renders `role="dialog"` and `aria-modal`; pass
/// `aria_labelledby` pointing at its [`SheetTitle`]. A `role` you pass
/// replaces both.
#[component]
pub fn Sheet(
    /// `.sheet-wide`, for settings.
    #[props(default)]
    wide: bool,
    /// Draws the `.sheet-grabber` handle at the top.
    #[props(default)]
    grabber: bool,
    #[props(default)] class: Option<String>,
    /// Keys pressed inside the sheet (Escape to close, arrow keys).
    #[props(default)]
    onkeydown: Option<EventHandler<KeyboardEvent>>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([
        Some("sheet"),
        wide.then_some("sheet-wide"),
        class.as_deref(),
    ]);
    let own_role = !caller_sets(&attributes, "role");
    rsx! {
        div {
            class,
            role: own_role.then_some("dialog"),
            "aria-modal": own_role.then_some("true"),
            onclick: move |evt| evt.stop_propagation(),
            onkeydown: move |evt| {
                if let Some(handler) = &onkeydown {
                    handler.call(evt);
                }
            },
            ..attributes,
            if grabber {
                div { class: "sheet-grabber", "aria-hidden": "true" }
            }
            {children}
        }
    }
}

macro_rules! part {
    ($(#[$doc:meta])* $name:ident, $tag:ident, $class:literal) => {
        $(#[$doc])*
        #[component]
        pub fn $name(
            #[props(default)] class: Option<String>,
            #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
            children: Element,
        ) -> Element {
            let class = with_extra($class, class.as_deref());
            rsx! {
                $tag { class, ..attributes, {children} }
            }
        }
    };
}

part!(
    /// The title row of a sheet (`.sheet-header`): title plus a close button.
    SheetHeader, div, "sheet-header"
);
part!(
    /// The sheet's title (`h2.sheet-title`).
    SheetTitle, h2, "sheet-title"
);
part!(
    /// The scrolling middle of a sheet (`.sheet-body`).
    SheetBody, div, "sheet-body"
);
part!(
    /// The action row of a sheet (`.sheet-footer`).
    SheetFooter, div, "sheet-footer"
);
part!(
    /// The alert's title (`h2.alert-title`).
    AlertTitle, h2, "alert-title"
);
part!(
    /// The alert's message (`p.alert-message`).
    AlertMessage, p, "alert-message"
);
part!(
    /// The alert's buttons (`.alert-actions`). They share the width.
    AlertActions, div, "alert-actions"
);
part!(
    /// A fixed stack at the top of the screen for toasts (`.toast-region`).
    /// Give it `role: "status"` if the toasts should be announced.
    ToastRegion, div, "toast-region"
);

/// One message (`.toast`). The HIG has no toast; this one is ours.
///
/// Regular glass by default. `solid` gives `.toast-solid`, an opaque raised
/// fill, for a toast over live video next to Clear glass.
#[component]
pub fn Toast(
    #[props(default)] solid: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = toast_class(solid, class.as_deref());
    rsx! {
        div { class, ..attributes, {children} }
    }
}

part!(
    /// A label above a group of menu items (`.menu-header`).
    MenuHeader, div, "menu-header"
);

/// A small centered dialog (`.alert`), `role="alertdialog"` with
/// `aria-modal`. A `role` you pass replaces both (`"dialog"` for a dialog
/// that asks for input rather than warning).
#[component]
pub fn Alert(
    #[props(default)] class: Option<String>,
    /// Keys pressed inside the alert (Escape to close).
    #[props(default)]
    onkeydown: Option<EventHandler<KeyboardEvent>>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("alert", class.as_deref());
    let own_role = !caller_sets(&attributes, "role");
    rsx! {
        div {
            class,
            role: own_role.then_some("alertdialog"),
            "aria-modal": own_role.then_some("true"),
            onclick: move |evt| evt.stop_propagation(),
            onkeydown: move |evt| {
                if let Some(handler) = &onkeydown {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
        }
    }
}

/// A popover list (`.menu`), `role="menu"`. Position it yourself.
///
/// `role="menu"` promises arrow-key navigation, which this component does
/// not provide. A menu without it passes `role: None::<&str>`, and its items
/// do the same.
#[component]
pub fn Menu(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("menu", class.as_deref());
    let role = (!caller_sets(&attributes, "role")).then_some("menu");
    rsx! {
        div { class, role, ..attributes, {children} }
    }
}

/// One menu entry (`.menu-item`).
///
/// `checked: Some(_)` makes it a `menuitemcheckbox` with `aria-checked`.
/// `destructive` adds `.menu-item-destructive` (red). A `role` you pass
/// replaces ours, as on [`Menu`].
#[component]
pub fn MenuItem(
    #[props(default)] destructive: bool,
    #[props(default)] disabled: bool,
    #[props(default)] checked: Option<bool>,
    #[props(default)] class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = join_classes([
        Some("menu-item"),
        destructive.then_some("menu-item-destructive"),
        class.as_deref(),
    ]);
    let role = if caller_sets(&attributes, "role") {
        None
    } else if checked.is_some() {
        Some("menuitemcheckbox")
    } else {
        Some("menuitem")
    };
    rsx! {
        button {
            r#type: "button",
            class,
            role,
            disabled,
            "aria-checked": checked.map(|on| if on { "true" } else { "false" }),
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

/// A line between groups of menu items (`.menu-separator`),
/// `role="separator"`. A `role` you pass replaces it, as on [`Menu`].
#[component]
pub fn MenuSeparator(
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let class = with_extra("menu-separator", class.as_deref());
    let role = (!caller_sets(&attributes, "role")).then_some("separator");
    rsx! {
        div { class, role, ..attributes }
    }
}

/// A tooltip chip (`.tooltip`), `role="tooltip"`.
///
/// Put it inside a positioned control (any button here is). It shows on
/// hover and keyboard focus of that control, or always with `open`. Give a
/// control at a screen edge `align: TooltipAlign::Start` or `End`.
#[component]
pub fn Tooltip(
    #[props(default)] placement: TooltipPlacement,
    #[props(default)] align: TooltipAlign,
    #[props(default)] open: bool,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = tooltip_class(placement, align, open, class.as_deref());
    rsx! {
        span { class, role: "tooltip", ..attributes, {children} }
    }
}
