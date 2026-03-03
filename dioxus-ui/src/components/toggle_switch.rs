// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// Reusable iOS-style toggle switch component.
///
/// # Props
/// - `enabled` – current on/off state
/// - `on_toggle` – called with the *new* value when clicked
/// - `width` / `height` – pill dimensions in px (defaults: 44×24)
#[component]
pub fn ToggleSwitch(
    enabled: bool,
    on_toggle: EventHandler<bool>,
    #[props(default = 44)] width: u32,
    #[props(default = 24)] height: u32,
) -> Element {
    let knob_size = height.saturating_sub(4);
    let knob_left_on = width.saturating_sub(knob_size + 2);
    let border_radius = height / 2;

    rsx! {
        button {
            r#type: "button",
            role: "switch",
            aria_checked: "{enabled}",
            style: format!(
                "position: relative; width: {width}px; height: {height}px; border-radius: {border_radius}px; \
                 border: none; cursor: pointer; background: {}; transition: background 0.2s; flex-shrink: 0;",
                if enabled { "#34c759" } else { "#636366" }
            ),
            onclick: move |_| {
                on_toggle.call(!enabled);
            },
            div {
                style: format!(
                    "position: absolute; top: 2px; left: {}px; width: {knob_size}px; height: {knob_size}px; \
                     border-radius: 50%; background: white; transition: left 0.2s;",
                    if enabled { knob_left_on } else { 2 }
                ),
            }
        }
    }
}
