/*
 * Copyright 2026 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Issue 2693: the persistent stand-in for a hidden self view.

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;

use crate::components::self_view::{
    self_view_hidden_icon_dock_modifier, self_view_hidden_icon_visible, self_view_tooltip_reveal,
    SELF_VIEW_HIDDEN_TOOLTIP_REVEAL_MS,
};
use crate::context::{save_self_view_visible, DockPosition, DockPositionCtx, SelfViewVisibleCtx};

const SELF_VIEW_HIDDEN_HINT_ID: &str = "self-view-hidden-hint";
const SELF_VIEW_SHOW_LABEL: &str = "Show self view";
const SELF_VIEW_SHOW_SCOPE: &str = "Only affects your view — others still see you.";

/// Persistent bottom-right control offering a one-press return of the self
/// view, drawn as an action-bar button. Emits ZERO element nodes while the self
/// view is visible.
#[component]
pub fn SelfViewHiddenPill(
    /// The same gate the self-view nav carries.
    can_stream: bool,
    /// Whether the hide toast is on screen; it already explains the icon.
    toast_present: bool,
    /// Whether the settings modal is covering the corner.
    settings_open: bool,
    /// Fired after the preference is flipped and persisted.
    on_show: EventHandler<()>,
) -> Element {
    // `try_use_context` is itself a hook, so both lookups precede any early
    // return. An isolated component test renders without either provider.
    let visible_ctx = try_use_context::<SelfViewVisibleCtx>();
    let dock_ctx = try_use_context::<DockPositionCtx>();

    // A prop is not a signal, so a flip of one never wakes the effect's
    // reactive context on its own. These mirrors are what wake it.
    let mut can_stream_now = use_signal(|| can_stream);
    let mut settings_now = use_signal(|| settings_open);
    let mut tooltip_open = use_signal(|| false);
    let mut reveal_timer = use_signal(|| None::<Timeout>);
    let mut shown_before = use_signal(|| false);
    let mut reveal_pending = use_signal(|| false);

    use_effect(move || {
        let shown = visible_ctx
            .is_some_and(|ctx| self_view_hidden_icon_visible((ctx.0)(), can_stream_now()));
        let decision = self_view_tooltip_reveal(
            shown,
            *shown_before.peek(),
            toast_present,
            settings_now(),
            *reveal_pending.peek(),
        );
        shown_before.set(shown);
        reveal_pending.set(decision.pending);
        if decision.open {
            tooltip_open.set(true);
            // Held in a signal, so it is cancelled by drop on unmount.
            reveal_timer.set(Some(Timeout::new(
                SELF_VIEW_HIDDEN_TOOLTIP_REVEAL_MS,
                move || tooltip_open.set(false),
            )));
        } else if !shown {
            tooltip_open.set(false);
            reveal_timer.set(None);
        }
    });

    // `peek`, so this component does not subscribe to its own mirrors and spin.
    if *can_stream_now.peek() != can_stream {
        can_stream_now.set(can_stream);
    }
    if *settings_now.peek() != settings_open {
        settings_now.set(settings_open);
    }

    let Some(ctx) = visible_ctx else {
        return rsx! {};
    };
    let mut self_view_visible = ctx.0;
    let dock = dock_ctx
        .map(|ctx| (ctx.0)())
        .unwrap_or(DockPosition::Bottom);

    if !self_view_hidden_icon_visible(self_view_visible(), can_stream) {
        return rsx! {};
    }

    let dock_modifier = self_view_hidden_icon_dock_modifier(dock);

    rsx! {
        // A control, not a status: no `role`/`aria-live`. The toast announces
        // the hide; this is what undoes it.
        button {
            r#type: "button",
            class: "video-control-button self-view-hidden-icon {dock_modifier}",
            "data-testid": "self-view-show-button",
            "data-tooltip-open": if tooltip_open() { "true" } else { "false" },
            "aria-label": SELF_VIEW_SHOW_LABEL,
            "aria-describedby": SELF_VIEW_HIDDEN_HINT_ID,
            onclick: move |evt: MouseEvent| {
                // Grid-overlay control, not a grid click — issue 1790.
                evt.stop_propagation();
                self_view_visible.set(true);
                save_self_view_visible(true);
                on_show.call(());
            },

            // Picture-in-picture, never a camera: this does not touch the camera.
            svg {
                view_box: "0 0 24 24",
                fill: "none",
                "aria-hidden": "true",
                rect {
                    x: "3",
                    y: "4",
                    width: "18",
                    height: "16",
                    rx: "2",
                    stroke: "currentColor",
                    stroke_width: "2",
                }
                rect {
                    x: "12",
                    y: "12",
                    width: "6",
                    height: "5",
                    rx: "1",
                    fill: "currentColor",
                }
            }

            span { class: "tooltip", "aria-hidden": "true",
                span { class: "tooltip-title", "{SELF_VIEW_SHOW_LABEL}" }
                span { class: "tooltip-desc", "{SELF_VIEW_SHOW_SCOPE}" }
            }

            // The tooltip is `visibility: hidden` until hover, so the
            // description AT reads is its own node.
            span {
                class: "visually-hidden",
                id: SELF_VIEW_HIDDEN_HINT_ID,
                "{SELF_VIEW_SHOW_SCOPE}"
            }
        }
    }
}
