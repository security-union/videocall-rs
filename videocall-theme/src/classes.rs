// SPDX-License-Identifier: MIT OR Apache-2.0

//! Enum to class-string mapping for every primitive.
//!
//! Plain Rust with no Dioxus in it, so the mapping is tested by `cargo test`
//! on the host without a browser. The components call these functions and do
//! nothing else with class names.

/// Joins class names with single spaces, skipping `None` and blank parts.
///
/// The caller's `class` prop goes through here too, so `class: ""` or a
/// class with stray spaces never leaves a double space behind.
pub fn join_classes<'a>(parts: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut out = String::new();
    for part in parts.into_iter().flatten() {
        for name in part.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(name);
        }
    }
    out
}

/// Look of a [`crate::Button`]. Maps to the `.btn-*` modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonVariant {
    /// `.btn`: gray fill. The secondary button.
    #[default]
    Default,
    /// `.btn-prominent`: accent fill. One or two per view.
    Prominent,
    /// `.btn-tinted`: accent-tinted fill, accent label.
    Tinted,
    /// `.btn-plain`: no fill, accent label.
    Plain,
    /// `.btn-destructive`: red-tinted fill, red label.
    Destructive,
    /// `.btn-destructive.btn-prominent`: solid red.
    DestructiveProminent,
    /// `.btn-destructive.btn-plain`: red text only.
    DestructivePlain,
    /// `.btn-glass`: Regular glass, for a button floating on content.
    Glass,
    /// `.btn-glass-clear`: Clear glass, for a button floating on video.
    GlassClear,
}

impl ButtonVariant {
    /// Every variant, in contract order.
    pub const ALL: [Self; 9] = [
        Self::Default,
        Self::Prominent,
        Self::Tinted,
        Self::Plain,
        Self::Destructive,
        Self::DestructiveProminent,
        Self::DestructivePlain,
        Self::Glass,
        Self::GlassClear,
    ];

    /// The modifier classes, or `None` for the default button.
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Prominent => Some("btn-prominent"),
            Self::Tinted => Some("btn-tinted"),
            Self::Plain => Some("btn-plain"),
            Self::Destructive => Some("btn-destructive"),
            Self::DestructiveProminent => Some("btn-destructive btn-prominent"),
            Self::DestructivePlain => Some("btn-destructive btn-plain"),
            Self::Glass => Some("btn-glass"),
            Self::GlassClear => Some("btn-glass-clear"),
        }
    }
}

/// Height of a [`crate::Button`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonSize {
    /// `.btn-sm`: 32px tall, 44px hit area.
    Small,
    /// The default 44px control.
    #[default]
    Medium,
    /// `.btn-lg`: 52px tall.
    Large,
}

impl ButtonSize {
    /// Every size, small to large.
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    /// The size class, or `None` for the default height.
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Small => Some("btn-sm"),
            Self::Medium => None,
            Self::Large => Some("btn-lg"),
        }
    }
}

/// The `type` attribute of a rendered `<button>`.
///
/// Defaults to `Button` so a primitive inside a `<form>` never submits it by
/// accident, which is what a bare `<button>` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ButtonType {
    #[default]
    Button,
    Submit,
    Reset,
}

impl ButtonType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Submit => "submit",
            Self::Reset => "reset",
        }
    }
}

/// Class string for a [`crate::Button`].
pub fn button_class(
    variant: ButtonVariant,
    size: ButtonSize,
    block: bool,
    extra: Option<&str>,
) -> String {
    join_classes([
        Some("btn"),
        variant.class(),
        size.class(),
        block.then_some("btn-block"),
        extra,
    ])
}

/// Look of an [`crate::IconButton`]. Maps to the `.icon-btn-*` modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IconButtonVariant {
    /// `.icon-btn`: no fill. Close, back, menu.
    #[default]
    Plain,
    /// `.icon-btn-filled`: gray fill.
    Filled,
    /// `.icon-btn-glass`: Regular glass.
    Glass,
    /// `.icon-btn-glass-clear`: Clear glass, over video.
    GlassClear,
}

impl IconButtonVariant {
    pub const ALL: [Self; 4] = [Self::Plain, Self::Filled, Self::Glass, Self::GlassClear];

    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Filled => Some("icon-btn-filled"),
            Self::Glass => Some("icon-btn-glass"),
            Self::GlassClear => Some("icon-btn-glass-clear"),
        }
    }
}

/// Size of an [`crate::IconButton`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IconButtonSize {
    /// 32px visual, 44px hit area (`.icon-btn-sm`).
    Small,
    /// 44px.
    #[default]
    Medium,
}

impl IconButtonSize {
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Small => Some("icon-btn-sm"),
            Self::Medium => None,
        }
    }
}

/// Class string for an [`crate::IconButton`].
pub fn icon_button_class(
    variant: IconButtonVariant,
    size: IconButtonSize,
    extra: Option<&str>,
) -> String {
    join_classes([Some("icon-btn"), variant.class(), size.class(), extra])
}

/// State of an in-call [`crate::ControlButton`] (`.video-control-button`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ControlState {
    /// The device is off: clear glass.
    #[default]
    Off,
    /// The device is on: inverted fill (`.active`).
    On,
    /// Hang up: red fill (`.danger`).
    Danger,
}

impl ControlState {
    pub const ALL: [Self; 3] = [Self::Off, Self::On, Self::Danger];

    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::On => Some("active"),
            Self::Danger => Some("danger"),
        }
    }
}

/// Class string for a [`crate::ControlButton`]. The `video-control-button`
/// name is kept because tests select it. `disabled` adds `.disabled`;
/// `unavailable` (a missing or blocked device) adds `.error` instead.
pub fn control_button_class(
    state: ControlState,
    disabled: bool,
    unavailable: bool,
    extra: Option<&str>,
) -> String {
    join_classes([
        Some("video-control-button"),
        state.class(),
        unavailable.then_some("error"),
        disabled.then_some("disabled"),
        extra,
    ])
}

/// Which glass a [`crate::Glass`] surface uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Material {
    /// `.glass-regular`. The default.
    #[default]
    Regular,
    /// `.glass-clear`. Over video or photos only.
    Clear,
}

impl Material {
    pub fn class(self) -> &'static str {
        match self {
            Self::Regular => "glass-regular",
            Self::Clear => "glass-clear",
        }
    }
}

/// Where a [`crate::MediaDim`] layer darkens the media.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DimEdge {
    /// `.media-dim`: the whole picture, 35% black.
    #[default]
    Full,
    /// `.media-dim-top`: a fade from the top edge.
    Top,
    /// `.media-dim-bottom`: a fade from the bottom edge.
    Bottom,
}

impl DimEdge {
    pub fn class(self) -> &'static str {
        match self {
            Self::Full => "media-dim",
            Self::Top => "media-dim-top",
            Self::Bottom => "media-dim-bottom",
        }
    }
}

/// Tone of a [`crate::Badge`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BadgeTone {
    /// Gray fill.
    #[default]
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
    /// `.badge-count`: the red count bubble.
    Count,
}

impl BadgeTone {
    pub const ALL: [Self; 6] = [
        Self::Neutral,
        Self::Accent,
        Self::Success,
        Self::Warning,
        Self::Danger,
        Self::Count,
    ];

    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Neutral => None,
            Self::Accent => Some("badge-accent"),
            Self::Success => Some("badge-success"),
            Self::Warning => Some("badge-warning"),
            Self::Danger => Some("badge-danger"),
            Self::Count => Some("badge-count"),
        }
    }
}

/// Tone of a [`crate::StatusDot`]. Neutral is gray.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatusTone {
    #[default]
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

impl StatusTone {
    pub const ALL: [Self; 5] = [
        Self::Neutral,
        Self::Accent,
        Self::Success,
        Self::Warning,
        Self::Danger,
    ];

    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Neutral => None,
            Self::Accent => Some("status-dot-accent"),
            Self::Success => Some("status-dot-success"),
            Self::Warning => Some("status-dot-warning"),
            Self::Danger => Some("status-dot-danger"),
        }
    }
}

/// Which side of its control a [`crate::Tooltip`] sits on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TooltipPlacement {
    #[default]
    Above,
    /// `.tooltip-below`.
    Below,
}

impl TooltipPlacement {
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Above => None,
            Self::Below => Some("tooltip-below"),
        }
    }
}

/// How a [`crate::Tooltip`] lines up with its control.
///
/// Centered is right almost everywhere. A control at the left or right edge
/// of the screen takes `Start` or `End`, so the tooltip grows away from the
/// edge instead of hanging over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TooltipAlign {
    #[default]
    Center,
    /// `.tooltip-start`: the control's leading edge.
    Start,
    /// `.tooltip-end`: the control's trailing edge.
    End,
}

impl TooltipAlign {
    pub const ALL: [Self; 3] = [Self::Center, Self::Start, Self::End];

    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Center => None,
            Self::Start => Some("tooltip-start"),
            Self::End => Some("tooltip-end"),
        }
    }
}

/// Class string for a [`crate::Tooltip`].
pub fn tooltip_class(
    placement: TooltipPlacement,
    align: TooltipAlign,
    open: bool,
    extra: Option<&str>,
) -> String {
    join_classes([
        Some("tooltip"),
        placement.class(),
        align.class(),
        open.then_some("is-open"),
        extra,
    ])
}

/// Class string for a [`crate::Toast`]. `solid` is `.toast-solid`, the
/// opaque fill for a toast over live video.
pub fn toast_class(solid: bool, extra: Option<&str>) -> String {
    join_classes([Some("toast"), solid.then_some("toast-solid"), extra])
}

/// Class string for a [`crate::Tab`]. A link tab has no `aria-selected`, so
/// it is marked with `.active` instead.
pub fn tab_class(selected_by_class: bool, extra: Option<&str>) -> String {
    join_classes([Some("tab"), selected_by_class.then_some("active"), extra])
}

/// Shorthand for a fixed base class plus the caller's `class`.
pub fn with_extra(base: &str, extra: Option<&str>) -> String {
    join_classes([Some(base), extra])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_skips_none_and_blank_and_normalizes_spaces() {
        assert_eq!(
            join_classes([Some("a"), None, Some(""), Some("  b  c ")]),
            "a b c"
        );
        assert_eq!(join_classes([None, None]), "");
    }

    #[test]
    fn default_button_is_just_btn() {
        assert_eq!(
            button_class(ButtonVariant::Default, ButtonSize::Medium, false, None),
            "btn"
        );
    }

    #[test]
    fn every_button_variant_maps_to_its_contract_class() {
        let expected = [
            "btn",
            "btn btn-prominent",
            "btn btn-tinted",
            "btn btn-plain",
            "btn btn-destructive",
            "btn btn-destructive btn-prominent",
            "btn btn-destructive btn-plain",
            "btn btn-glass",
            "btn btn-glass-clear",
        ];
        for (variant, want) in ButtonVariant::ALL.into_iter().zip(expected) {
            assert_eq!(
                button_class(variant, ButtonSize::Medium, false, None),
                want,
                "{variant:?}"
            );
        }
    }

    #[test]
    fn button_size_block_and_extra_compose_in_order() {
        assert_eq!(
            button_class(
                ButtonVariant::Prominent,
                ButtonSize::Large,
                true,
                Some("cta")
            ),
            "btn btn-prominent btn-lg btn-block cta"
        );
        assert_eq!(
            button_class(ButtonVariant::Plain, ButtonSize::Small, false, None),
            "btn btn-plain btn-sm"
        );
    }

    #[test]
    fn button_type_defaults_to_button() {
        assert_eq!(ButtonType::default().as_str(), "button");
        assert_eq!(ButtonType::Submit.as_str(), "submit");
    }

    #[test]
    fn icon_button_classes() {
        let expected = [
            "icon-btn",
            "icon-btn icon-btn-filled",
            "icon-btn icon-btn-glass",
            "icon-btn icon-btn-glass-clear",
        ];
        for (variant, want) in IconButtonVariant::ALL.into_iter().zip(expected) {
            assert_eq!(
                icon_button_class(variant, IconButtonSize::Medium, None),
                want
            );
        }
        assert_eq!(
            icon_button_class(
                IconButtonVariant::Filled,
                IconButtonSize::Small,
                Some("close-button")
            ),
            "icon-btn icon-btn-filled icon-btn-sm close-button"
        );
    }

    #[test]
    fn control_button_keeps_the_tested_name() {
        assert_eq!(
            control_button_class(ControlState::Off, false, false, None),
            "video-control-button"
        );
        assert_eq!(
            control_button_class(ControlState::On, false, false, None),
            "video-control-button active"
        );
        assert_eq!(
            control_button_class(ControlState::Danger, true, false, Some("x")),
            "video-control-button danger disabled x"
        );
        // A missing device: `.error` and no `.disabled`, as dioxus-ui's mic
        // and camera buttons render it.
        assert_eq!(
            control_button_class(ControlState::On, false, true, None),
            "video-control-button active error"
        );
    }

    #[test]
    fn materials_and_dims() {
        assert_eq!(Material::default().class(), "glass-regular");
        assert_eq!(Material::Clear.class(), "glass-clear");
        assert_eq!(DimEdge::Full.class(), "media-dim");
        assert_eq!(DimEdge::Top.class(), "media-dim-top");
        assert_eq!(DimEdge::Bottom.class(), "media-dim-bottom");
    }

    #[test]
    fn badge_and_status_tones() {
        let badges: Vec<_> = BadgeTone::ALL
            .into_iter()
            .map(|tone| join_classes([Some("badge"), tone.class()]))
            .collect();
        assert_eq!(
            badges,
            [
                "badge",
                "badge badge-accent",
                "badge badge-success",
                "badge badge-warning",
                "badge badge-danger",
                "badge badge-count"
            ]
        );
        let dots: Vec<_> = StatusTone::ALL
            .into_iter()
            .map(|tone| join_classes([Some("status-dot"), tone.class()]))
            .collect();
        assert_eq!(
            dots,
            [
                "status-dot",
                "status-dot status-dot-accent",
                "status-dot status-dot-success",
                "status-dot status-dot-warning",
                "status-dot status-dot-danger"
            ]
        );
    }

    #[test]
    fn tooltip_and_tab() {
        assert_eq!(
            tooltip_class(TooltipPlacement::Above, TooltipAlign::Center, false, None),
            "tooltip"
        );
        assert_eq!(
            tooltip_class(TooltipPlacement::Below, TooltipAlign::Center, true, None),
            "tooltip tooltip-below is-open"
        );
        assert_eq!(
            tooltip_class(TooltipPlacement::Above, TooltipAlign::Start, false, None),
            "tooltip tooltip-start"
        );
        assert_eq!(
            tooltip_class(TooltipPlacement::Below, TooltipAlign::End, true, Some("x")),
            "tooltip tooltip-below tooltip-end is-open x"
        );
        assert_eq!(toast_class(false, None), "toast");
        assert_eq!(
            toast_class(true, Some("peer-toast")),
            "toast toast-solid peer-toast"
        );
        assert_eq!(tab_class(false, None), "tab");
        assert_eq!(
            tab_class(true, Some("settings-nav-button")),
            "tab active settings-nav-button"
        );
    }
}
