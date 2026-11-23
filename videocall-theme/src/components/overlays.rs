// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scrim, sheet, alert, menu, toast and tooltip.
//!
//! These only draw. Opening, closing, focus and timers belong to the caller.
//! One small behavior is built in: a click inside a [`Sheet`] or [`Alert`]
//! does not reach the [`Scrim`] around it, so `Scrim { onclick }` fires only
//! for clicks on the backdrop.

use dioxus::prelude::*;

use super::{caller_sets, part};
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

part!(
    /// The title row of a sheet (`.sheet-header`): title plus a close button.
    SheetHeader, part_div, "sheet-header"
);
part!(
    /// The sheet's title (`h2.sheet-title`).
    SheetTitle, part_h2, "sheet-title"
);
part!(
    /// The scrolling middle of a sheet (`.sheet-body`).
    SheetBody, part_div, "sheet-body"
);
part!(
    /// The action row of a sheet (`.sheet-footer`).
    SheetFooter, part_div, "sheet-footer"
);
part!(
    /// The alert's title (`h2.alert-title`).
    AlertTitle, part_h2, "alert-title"
);
part!(
    /// The alert's message (`p.alert-message`).
    AlertMessage, part_p, "alert-message"
);
part!(
    /// The alert's buttons (`.alert-actions`). They share the width.
    AlertActions, part_div, "alert-actions"
);
part!(
    /// A fixed stack at the top of the screen for toasts (`.toast-region`).
    /// Give it `role: "status"` if the toasts should be announced.
    ToastRegion, part_div, "toast-region"
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
    MenuHeader, part_div, "menu-header"
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
/// With its own role it also does the menu keys: Down and Up move focus to
/// the next and previous item (wrapping), Home and End to the first and
/// last, and Escape or Tab call `onclose`. Disabled items are skipped. The
/// caller opens it, focuses the first item (`MenuItem { autofocus }`) and,
/// in `onclose`, hides it and returns focus to the button that opened it.
///
/// A list that is not a menu (no arrow keys wanted) passes
/// `role: None::<&str>`, and its items do the same; it then gets no key
/// handling either.
#[component]
pub fn Menu(
    #[props(default)] class: Option<String>,
    /// Escape or Tab inside the menu.
    #[props(default)]
    onclose: Option<EventHandler<()>>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("menu", class.as_deref());
    let own_role = !caller_sets(&attributes, "role");
    let role = own_role.then_some("menu");
    rsx! {
        div {
            class,
            role,
            onkeydown: move |evt: KeyboardEvent| {
                if !own_role {
                    return;
                }
                let step = match evt.key() {
                    Key::ArrowDown => MenuStep::Next,
                    Key::ArrowUp => MenuStep::Previous,
                    Key::Home => MenuStep::First,
                    Key::End => MenuStep::Last,
                    Key::Escape | Key::Tab => {
                        evt.prevent_default();
                        evt.stop_propagation();
                        if let Some(handler) = &onclose {
                            handler.call(());
                        }
                        return;
                    }
                    _ => return,
                };
                evt.prevent_default();
                move_menu_focus(step);
            },
            ..attributes,
            {children}
        }
    }
}

/// Where an arrow key sends focus inside a [`Menu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuStep {
    Next,
    Previous,
    First,
    Last,
}

impl MenuStep {
    /// The index that gets focus, out of `count` items, when the item at
    /// `current` has it (`None`: focus is on the menu itself).
    pub fn target(self, current: Option<u32>, count: u32) -> Option<u32> {
        if count == 0 {
            return None;
        }
        Some(match (self, current) {
            (Self::First, _) | (Self::Next, None) => 0,
            (Self::Last, _) | (Self::Previous, None) => count - 1,
            (Self::Next, Some(index)) => (index + 1) % count,
            (Self::Previous, Some(index)) => (index + count - 1) % count,
        })
    }
}

/// Moves DOM focus inside the menu that holds the focused element. It reads
/// the DOM at key-press time, so items added or removed while the menu is
/// open are counted.
fn move_menu_focus(step: MenuStep) {
    use wasm_bindgen::JsCast;

    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Some(active) = document.active_element() else {
        return;
    };
    let Ok(Some(menu)) = active.closest(r#"[role="menu"]"#) else {
        return;
    };
    let Ok(items) = menu.query_selector_all(
        r#"[role="menuitem"]:not(:disabled), [role="menuitemcheckbox"]:not(:disabled), [role="menuitemradio"]:not(:disabled)"#,
    ) else {
        return;
    };
    let count = items.length();
    let current = (0..count).find(|&index| {
        items
            .item(index)
            .is_some_and(|item| item.is_same_node(Some(active.as_ref())))
    });
    let Some(target) = step.target(current, count) else {
        return;
    };
    if let Some(item) = items
        .item(target)
        .and_then(|item| item.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = item.focus();
    }
}

/// One menu entry (`.menu-item`).
///
/// `checked: Some(_)` makes it a `menuitemcheckbox` with `aria-checked`.
/// `destructive` adds `.menu-item-destructive` (red). `autofocus` focuses
/// it once it is in the DOM, which is how a menu that just opened puts
/// focus on its first item. A `role` you pass replaces ours, as on
/// [`Menu`].
#[component]
pub fn MenuItem(
    #[props(default)] destructive: bool,
    #[props(default)] disabled: bool,
    #[props(default)] checked: Option<bool>,
    #[props(default)] autofocus: bool,
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
            onmounted: move |evt: MountedEvent| async move {
                if autofocus {
                    let _ = evt.set_focus(true).await;
                }
            },
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
