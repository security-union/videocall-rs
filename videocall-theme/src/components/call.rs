// SPDX-License-Identifier: MIT OR Apache-2.0

//! The in-call control bar's controls, described once.
//!
//! dioxus-ui and the gallery both render the bar from [`CallControl`], so a
//! label, tooltip, icon or state can't differ between the design guide and
//! the app. The components only draw: what a click does, and which state a
//! control is in, belong to the caller.
//!
//! On a wide screen every control is a [`CallButton`] in the bar. On a phone
//! the bar keeps the controls where [`CallControl::in_phone_bar`] is true,
//! plus a [`CallMoreButton`], and the rest become [`CallMenuItem`]s in a
//! [`crate::Menu`] that the More button opens.

use dioxus::prelude::*;

use super::{ControlButton, Icon, MenuItem, Tooltip};
use crate::classes::{ControlState, TooltipAlign};
use crate::icons::{self, Glyph};

/// One control of the call bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallControl {
    Microphone,
    Camera,
    ScreenShare,
    BabyMode,
    Peers,
    Diagnostics,
    DeviceSettings,
    HangUp,
}

impl CallControl {
    /// Every control, in bar order.
    pub const ALL: [Self; 8] = [
        Self::Microphone,
        Self::Camera,
        Self::ScreenShare,
        Self::BabyMode,
        Self::Peers,
        Self::Diagnostics,
        Self::DeviceSettings,
        Self::HangUp,
    ];

    /// Whether the phone bar keeps this control. The others move into the
    /// More menu.
    pub fn in_phone_bar(self) -> bool {
        matches!(self, Self::Microphone | Self::Camera | Self::HangUp)
    }

    /// The accessible name. A toggle with `aria-pressed` (mic, camera,
    /// share) keeps a fixed name; the panels name the action, like their
    /// tooltips, because the tooltip is hidden from the name.
    pub fn label(self, on: bool) -> &'static str {
        match (self, on) {
            (Self::Microphone, _) => "Microphone",
            (Self::Camera, _) => "Camera",
            (Self::ScreenShare, _) => "Share screen",
            (Self::BabyMode, _) => "Lock controls (baby mode)",
            (Self::Peers, true) => "Close Peers",
            (Self::Peers, false) => "Open Peers",
            (Self::Diagnostics, true) => "Close Diagnostics",
            (Self::Diagnostics, false) => "Open Diagnostics",
            (Self::DeviceSettings, true) => "Close Settings",
            (Self::DeviceSettings, false) => "Device Settings",
            (Self::HangUp, _) => "Hang Up",
        }
    }

    /// What a click does, shown in the tooltip.
    pub fn tooltip(self, on: bool) -> &'static str {
        match (self, on) {
            (Self::Microphone, true) => "Mute",
            (Self::Microphone, false) => "Unmute",
            (Self::Camera, true) => "Stop Video",
            (Self::Camera, false) => "Start Video",
            (Self::ScreenShare, true) => "Stop Screen Share",
            (Self::ScreenShare, false) => "Share Screen",
            _ => self.label(on),
        }
    }

    pub fn glyph(self, on: bool) -> &'static Glyph {
        match (self, on) {
            (Self::Microphone, true) => &icons::MIC,
            (Self::Microphone, false) => &icons::MIC_OFF,
            (Self::Camera, true) => &icons::CAMERA,
            (Self::Camera, false) => &icons::CAMERA_OFF,
            (Self::ScreenShare, true) => &icons::SCREEN,
            (Self::ScreenShare, false) => &icons::SCREEN_OFF,
            (Self::BabyMode, _) => &icons::BABY,
            (Self::Peers, true) => &icons::PEOPLE,
            (Self::Peers, false) => &icons::PEOPLE_OFF,
            // The open panel shows the crossed-out line: a click closes it.
            (Self::Diagnostics, true) => &icons::ACTIVITY_OFF,
            (Self::Diagnostics, false) => &icons::ACTIVITY,
            (Self::DeviceSettings, _) => &icons::SETTINGS,
            (Self::HangUp, _) => &icons::HANG_UP,
        }
    }

    /// The button's look: on is inverted, hang up is red.
    pub fn state(self, on: bool) -> ControlState {
        match self {
            Self::HangUp => ControlState::Danger,
            Self::BabyMode => ControlState::Off,
            _ if on => ControlState::On,
            _ => ControlState::Off,
        }
    }

    /// `aria-pressed` in the bar, for the device toggles.
    pub fn pressed(self, on: bool) -> Option<bool> {
        matches!(self, Self::Microphone | Self::Camera | Self::ScreenShare).then_some(on)
    }

    /// `aria-checked` in the More menu, for anything with an on state.
    pub fn checked(self, on: bool) -> Option<bool> {
        (!matches!(self, Self::BabyMode | Self::HangUp)).then_some(on)
    }

    /// The first and last buttons line their tooltips up with the screen
    /// edge instead of hanging over it.
    pub fn tooltip_align(self) -> TooltipAlign {
        match self {
            Self::Microphone => TooltipAlign::Start,
            Self::HangUp => TooltipAlign::End,
            _ => TooltipAlign::Center,
        }
    }

    /// The bar button's `id`, where tests or the baby mode unlock need one.
    pub fn id(self) -> Option<&'static str> {
        match self {
            Self::BabyMode => Some("baby-mode-button"),
            _ => None,
        }
    }

    /// The bar button's `data-testid`.
    pub fn test_id(self) -> Option<&'static str> {
        match self {
            Self::BabyMode => Some("baby-mode-button"),
            Self::DeviceSettings => Some("open-settings"),
            _ => None,
        }
    }

    /// The `data-testid` of the control's row in the More menu.
    pub fn menu_test_id(self) -> &'static str {
        match self {
            Self::Microphone => "more-microphone",
            Self::Camera => "more-camera",
            Self::ScreenShare => "more-share-screen",
            Self::BabyMode => "more-baby-mode",
            Self::Peers => "more-peers",
            Self::Diagnostics => "more-diagnostics",
            Self::DeviceSettings => "more-device-settings",
            Self::HangUp => "more-hang-up",
        }
    }
}

/// A control as a bar button: a [`ControlButton`] with the control's name,
/// icon, tooltip and state.
///
/// `on` is the control's state (mic on, panel open). `unavailable` is a
/// missing or blocked device, `disabled` a control that is busy (share
/// while the picker is opening). `onclick` gets the control, so one
/// handler can serve every button and menu row.
#[component]
pub fn CallButton(
    control: CallControl,
    #[props(default)] on: bool,
    #[props(default)] unavailable: bool,
    #[props(default)] disabled: bool,
    onclick: EventHandler<CallControl>,
) -> Element {
    let label = control.label(on);
    let tooltip = control.tooltip(on);
    let glyph = control.glyph(on);
    let align = control.tooltip_align();
    rsx! {
        ControlButton {
            label,
            state: control.state(on),
            pressed: control.pressed(on),
            unavailable,
            disabled,
            id: control.id(),
            "data-testid": control.test_id(),
            onclick: move |_| onclick.call(control),
            // Hang up has always put its tooltip first; the order is kept so
            // the bar's markup did not change when it moved here.
            if control == CallControl::HangUp {
                Tooltip { align, "{tooltip}" }
                Icon { glyph, size: "24" }
            } else {
                Icon { glyph }
                Tooltip { align, "{tooltip}" }
            }
        }
    }
}

/// The phone bar's More button. It opens a [`crate::Menu`] of the controls
/// the phone bar leaves out.
///
/// `menu_id` is the menu's `id`, for `aria-controls` while it is open.
/// `onmounted` hands back the button, so the caller can return focus to it
/// when the menu closes.
#[component]
pub fn CallMoreButton(
    open: bool,
    #[props(default)] menu_id: Option<String>,
    onclick: EventHandler<MouseEvent>,
    #[props(default)] onmounted: Option<EventHandler<MountedEvent>>,
) -> Element {
    rsx! {
        ControlButton {
            label: "More",
            state: if open { ControlState::On } else { ControlState::Off },
            "aria-haspopup": "menu",
            "aria-expanded": if open { "true" } else { "false" },
            "aria-controls": if open { menu_id } else { None },
            onclick: move |evt| onclick.call(evt),
            onmounted: move |evt| {
                if let Some(handler) = &onmounted {
                    handler.call(evt);
                }
            },
            Icon { glyph: &icons::MORE }
            Tooltip { "More" }
        }
    }
}

/// A control as a row of the More menu: its icon and name, `aria-checked`
/// for one with an on state (and a check mark when on), the same name the
/// bar button has.
#[component]
pub fn CallMenuItem(
    control: CallControl,
    #[props(default)] on: bool,
    #[props(default)] disabled: bool,
    /// Focus this row when the menu opens (the first row).
    #[props(default)]
    autofocus: bool,
    onclick: EventHandler<CallControl>,
) -> Element {
    let label = control.label(on);
    let checked = control.checked(on);
    rsx! {
        MenuItem {
            checked,
            disabled,
            autofocus,
            "data-testid": control.menu_test_id(),
            onclick: move |_| onclick.call(control),
            Icon { glyph: control.glyph(on), "aria-hidden": "true" }
            span { class: "grow", "{label}" }
            if checked == Some(true) {
                Icon { glyph: &icons::CHECK, "aria-hidden": "true" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_bar_keeps_mic_camera_and_hang_up() {
        let kept: Vec<_> = CallControl::ALL
            .into_iter()
            .filter(|control| control.in_phone_bar())
            .collect();
        assert_eq!(
            kept,
            [
                CallControl::Microphone,
                CallControl::Camera,
                CallControl::HangUp
            ]
        );
    }

    #[test]
    fn toggles_keep_a_fixed_name_and_panels_name_the_action() {
        for on in [false, true] {
            assert_eq!(CallControl::Microphone.label(on), "Microphone");
            assert_eq!(CallControl::Camera.label(on), "Camera");
            assert_eq!(CallControl::ScreenShare.label(on), "Share screen");
        }
        assert_eq!(CallControl::Peers.label(false), "Open Peers");
        assert_eq!(CallControl::Peers.label(true), "Close Peers");
        assert_eq!(CallControl::Microphone.tooltip(true), "Mute");
        assert_eq!(CallControl::Peers.tooltip(true), "Close Peers");
    }

    #[test]
    fn only_the_device_toggles_are_pressed_and_only_actions_are_unchecked() {
        assert_eq!(CallControl::Microphone.pressed(true), Some(true));
        assert_eq!(CallControl::Peers.pressed(true), None);
        assert_eq!(CallControl::Peers.checked(true), Some(true));
        assert_eq!(CallControl::BabyMode.checked(false), None);
    }
}
