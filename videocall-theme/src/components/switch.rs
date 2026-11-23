// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

use crate::classes::with_extra;

/// Reusable iOS-style toggle switch component.
///
/// Ported from `dioxus-ui/src/components/toggle_switch.rs` with the same
/// role, ARIA and test id behavior, so dioxus-ui can re-export it as
/// `ToggleSwitch`. `width` and `height` became optional (the default
/// track is now 51x31 px, it was 44x24), and there are two additions:
/// `class`, and the usual pass-through of global attributes. Do not pass `style` through: the size override below
/// owns it.
///
/// All of the look (track, knob, colors, the on/off transition and the 44px
/// hit area) comes from the `.switch` class in `styles/components.css`. The
/// state is carried by `aria-checked`, which is also what the stylesheet keys
/// on, so there is no separate "on" class to keep in sync. The contract puts
/// a switch inside a list row, next to a visible label.
///
/// # Props
/// - `enabled` – current on/off state
/// - `on_toggle` – called with the *new* value when clicked
/// - `width` / `height` – track size in px. Leave them out to get the size
///   from the stylesheet, which is what every call site wants today.
/// - `test_id` – optional `data-testid` for automation
/// - `labelled_by` – optional id of the element that labels the switch
#[component]
pub fn Switch(
    enabled: bool,
    on_toggle: EventHandler<bool>,
    #[props(default)] width: Option<u32>,
    #[props(default)] height: Option<u32>,
    #[props(default = false)] disabled: bool,
    #[props(default)] test_id: Option<&'static str>,
    #[props(default)] labelled_by: Option<&'static str>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes, extends = button)] attributes: Vec<Attribute>,
) -> Element {
    let size_override = size_override_style(width, height);
    let class = with_extra("switch", class.as_deref());

    rsx! {
        button {
            r#type: "button",
            class,
            role: "switch",
            aria_checked: "{enabled}",
            "aria-labelledby": labelled_by,
            "data-testid": test_id,
            disabled: disabled,
            style: size_override,
            onclick: move |_| {
                if disabled {
                    return;
                }
                on_toggle.call(!enabled);
            },
            ..attributes,
        }
    }
}

/// Builds the inline style that overrides the stylesheet's track size.
///
/// Returns `None` when neither dimension is given, so the common case renders
/// no `style` attribute at all. Only the two size variables are ever written
/// here: colors stay in the stylesheet.
pub fn size_override_style(width: Option<u32>, height: Option<u32>) -> Option<String> {
    match (width, height) {
        (None, None) => None,
        (Some(width), None) => Some(format!("--switch-width: {width}px;")),
        (None, Some(height)) => Some(format!("--switch-height: {height}px;")),
        (Some(width), Some(height)) => Some(format!(
            "--switch-width: {width}px; --switch-height: {height}px;"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::size_override_style;

    #[test]
    fn no_dimensions_means_no_inline_style() {
        assert_eq!(size_override_style(None, None), None);
    }

    #[test]
    fn dimensions_become_size_variables_only() {
        assert_eq!(
            size_override_style(Some(60), None).as_deref(),
            Some("--switch-width: 60px;")
        );
        assert_eq!(
            size_override_style(None, Some(30)).as_deref(),
            Some("--switch-height: 30px;")
        );
        assert_eq!(
            size_override_style(Some(60), Some(30)).as_deref(),
            Some("--switch-width: 60px; --switch-height: 30px;")
        );
    }
}
