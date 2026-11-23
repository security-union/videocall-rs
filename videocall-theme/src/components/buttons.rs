// SPDX-License-Identifier: MIT OR Apache-2.0

//! `.btn`, `.icon-btn` and the in-call `.video-control-button`.

use dioxus::prelude::*;

use crate::classes::{
    button_class, control_button_class, icon_button_class, ButtonSize, ButtonType, ButtonVariant,
    ControlState, IconButtonSize, IconButtonVariant,
};

fn aria_bool(value: Option<bool>) -> Option<&'static str> {
    value.map(|on| if on { "true" } else { "false" })
}

/// A text button (`.btn` and its modifiers).
///
/// `pressed: Some(_)` makes it a toggle button (`aria-pressed`), which the
/// stylesheet draws tinted when on. Leave it `None` for an ordinary button.
#[component]
pub fn Button(
    #[props(default)] variant: ButtonVariant,
    #[props(default)] size: ButtonSize,
    /// Full width (`.btn-block`).
    #[props(default)]
    block: bool,
    #[props(default)] disabled: bool,
    #[props(default)] pressed: Option<bool>,
    #[props(default)] button_type: ButtonType,
    /// Extra classes, joined after the system ones.
    #[props(default)]
    class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = button_class(variant, size, block, class.as_deref());
    rsx! {
        button {
            r#type: button_type.as_str(),
            class,
            disabled,
            "aria-pressed": aria_bool(pressed),
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

/// A round 44px icon button (`.icon-btn`).
///
/// `label` becomes the `aria-label`, because an icon alone says nothing to a
/// screen reader. Leave it out only when something else names the button,
/// such as a `title` (dioxus-ui's change-name button) or visible text. Put
/// the SVG (and an optional [`crate::Tooltip`]) in the children.
#[component]
pub fn IconButton(
    /// Accessible name, rendered as `aria-label`.
    #[props(default)]
    label: Option<String>,
    #[props(default)] variant: IconButtonVariant,
    #[props(default)] size: IconButtonSize,
    #[props(default)] disabled: bool,
    #[props(default)] pressed: Option<bool>,
    #[props(default)] button_type: ButtonType,
    #[props(default)] class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = icon_button_class(variant, size, class.as_deref());
    rsx! {
        button {
            r#type: button_type.as_str(),
            class,
            disabled,
            "aria-label": label,
            "aria-pressed": aria_bool(pressed),
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

/// The in-call control (`.video-control-button`): round, Clear glass.
///
/// Two ways to be out of reach, matching what dioxus-ui renders:
/// - `disabled`: the `disabled` attribute and the `.disabled` class. The
///   screen share button while the picker is opening.
/// - `unavailable`: the device is missing or blocked. The `disabled`
///   attribute, the `.error` class (no `.disabled`) and the small "!" badge
///   (`.device-warning`). The badge is decorative (`aria-hidden`); the
///   button's name and tooltip carry the meaning.
///
/// A disabled `<button>` leaves the tab order, so its tooltip shows on hover
/// only. Clicks are swallowed while it is disabled, in case a browser still
/// delivers one.
///
/// `label` becomes the `aria-label`. A toggle (mic, camera, screen share)
/// takes a fixed label plus `pressed`. Leave `label` out when the tooltip
/// text should name the button, as for hang up. Put the icon and a
/// [`crate::Tooltip`] in the children.
#[component]
pub fn ControlButton(
    /// Accessible name, rendered as `aria-label`.
    #[props(default)]
    label: Option<String>,
    #[props(default)] state: ControlState,
    #[props(default)] disabled: bool,
    #[props(default)] unavailable: bool,
    #[props(default)] pressed: Option<bool>,
    #[props(default)] class: Option<String>,
    #[props(default)] onclick: Option<EventHandler<MouseEvent>>,
    /// The button is in the DOM. Keep the handle to move focus back to it,
    /// as the More button does when its menu closes.
    #[props(default)]
    onmounted: Option<EventHandler<MountedEvent>>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = control_button_class(state, disabled, unavailable, class.as_deref());
    let blocked = disabled || unavailable;
    rsx! {
        button {
            r#type: "button",
            class,
            "aria-label": label,
            disabled: blocked,
            "aria-pressed": aria_bool(pressed),
            onclick: move |evt| {
                if blocked {
                    return;
                }
                if let Some(handler) = &onclick {
                    handler.call(evt);
                }
            },
            onmounted: move |evt| {
                if let Some(handler) = &onmounted {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
            if unavailable {
                span { class: "device-warning", "aria-hidden": "true", "!" }
            }
        }
    }
}
