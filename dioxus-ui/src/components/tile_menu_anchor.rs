// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pins a tile's host-actions menu to its trigger while the menu is open.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::animation_frame::AnimationFrame;
use crate::components::signal_quality::{element_rect, Rect};

const EDGE_MARGIN_PX: f64 = 8.0;
const TRIGGER_GAP_PX: f64 = 6.0;
const MIN_SIDE_PX: f64 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MenuPlacement {
    pub left: f64,
    pub top: f64,
    pub max_height: Option<f64>,
}

/// Right-aligned under `trigger`; above it when only that fits; otherwise on
/// the roomier side (below while above is under `MIN_SIDE_PX`), capped to that
/// side's height. Never over the trigger.
pub(crate) fn place_tile_menu(
    trigger: Rect,
    menu_w: f64,
    menu_h: f64,
    bounds: Rect,
) -> MenuPlacement {
    let left = (trigger.right - menu_w)
        .min(bounds.right - menu_w)
        .max(bounds.left);
    let below = trigger.bottom + TRIGGER_GAP_PX;
    let room_below = (bounds.bottom - below).max(0.0);
    let room_above = (trigger.top - TRIGGER_GAP_PX - bounds.top).max(0.0);
    let (top, max_height) = if menu_h <= room_below {
        (below, None)
    } else if menu_h <= room_above {
        (trigger.top - TRIGGER_GAP_PX - menu_h, None)
    } else if room_below >= room_above || room_above < MIN_SIDE_PX {
        (below, Some(room_below))
    } else {
        (trigger.top - TRIGGER_GAP_PX - room_above, Some(room_above))
    };
    MenuPlacement {
        left,
        top,
        max_height,
    }
}

/// The viewport, narrowed to the trigger's `#grid-container` when it has one
/// and cut off above a shown `footer`, inset by `EDGE_MARGIN_PX`.
fn menu_bounds(
    trigger: &web_sys::Element,
    win: &web_sys::Window,
    footer: Option<&web_sys::Element>,
) -> Rect {
    let size = |v: Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>| {
        v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
    };
    let mut b = Rect {
        left: 0.0,
        top: 0.0,
        right: size(win.inner_width()),
        bottom: size(win.inner_height()),
    };
    if let Ok(Some(stage)) = trigger.closest("#grid-container") {
        let s = element_rect(&stage);
        b = Rect {
            left: b.left.max(s.left),
            top: b.top.max(s.top),
            right: b.right.min(s.right),
            bottom: b.bottom.min(s.bottom),
        };
    }
    if let Some(f) = footer.map(element_rect).filter(|f| f.height() > 0.0) {
        b.bottom = b.bottom.min(f.top);
    }
    Rect {
        left: b.left + EDGE_MARGIN_PX,
        top: b.top + EDGE_MARGIN_PX,
        right: b.right - EDGE_MARGIN_PX,
        bottom: b.bottom - EDGE_MARGIN_PX,
    }
}

fn set_style(style: &web_sys::CssStyleDeclaration, property: &str, want: &str) {
    if style.get_property_value(property).ok().as_deref() == Some(want) {
        return;
    }
    if want.is_empty() {
        let _ = style.remove_property(property);
    } else {
        let _ = style.set_property(property, want);
    }
}

fn px(value: f64) -> String {
    format!("{value:.0}px")
}

fn find_footer() -> Option<web_sys::Element> {
    gloo_utils::document()
        .query_selector(".meeting-footer")
        .ok()
        .flatten()
}

fn place_menu(trigger_id: &str, menu_id: &str, footer: &mut Option<web_sys::Element>) {
    if footer.as_ref().is_some_and(|f| !f.is_connected()) {
        *footer = find_footer();
    }
    let doc = gloo_utils::document();
    let (Some(win), Some(trigger), Some(menu)) = (
        web_sys::window(),
        doc.get_element_by_id(trigger_id),
        doc.get_element_by_id(menu_id)
            .and_then(|m| m.dyn_into::<web_sys::HtmlElement>().ok()),
    ) else {
        return;
    };
    // Content height, not the `max-height`-capped box.
    let menu_h = f64::from(menu.scroll_height() + menu.offset_height() - menu.client_height());
    let placement = place_tile_menu(
        element_rect(&trigger),
        element_rect(&menu).width(),
        menu_h,
        menu_bounds(&trigger, &win, footer.as_ref()),
    );
    let style = menu.style();
    set_style(&style, "left", &px(placement.left));
    set_style(&style, "top", &px(placement.top));
    set_style(
        &style,
        "max-height",
        &placement.max_height.map(px).unwrap_or_default(),
    );
    set_style(&style, "visibility", "visible");
}

/// Renders nothing. While mounted it re-places `menu_id` against `trigger_id`
/// every animation frame; unmounting cancels the loop.
#[component]
pub(crate) fn TileMenuAnchor(trigger_id: String, menu_id: String) -> Element {
    use_hook(move || {
        let mut footer = find_footer();
        let raf = AnimationFrame::new_loop(move || place_menu(&trigger_id, &menu_id, &mut footer));
        raf.request();
        raf
    });
    rsx! {}
}

thread_local! {
    static LIVE_DISMISS_LISTENERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[doc(hidden)]
#[allow(dead_code)]
pub fn live_tile_menu_dismiss_listeners_for_test() -> usize {
    LIVE_DISMISS_LISTENERS.with(|n| n.get())
}

struct OutsidePointerDown(wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>);

impl Drop for OutsidePointerDown {
    fn drop(&mut self) {
        let _ = gloo_utils::document().remove_event_listener_with_callback_and_bool(
            "pointerdown",
            self.0.as_ref().unchecked_ref(),
            true,
        );
        LIVE_DISMISS_LISTENERS.with(|n| n.set(n.get() - 1));
    }
}

/// Renders nothing. While mounted, a pointerdown closes the menu unless it
/// lands inside the trigger's wrapper or on any `.tile-menu-backdrop`.
#[component]
pub(crate) fn TileMenuDismiss(trigger_id: String, show: Signal<bool>) -> Element {
    use_hook(move || {
        let mut show = show;
        let on_down = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
            move |e: web_sys::Event| {
                let Some(target) = e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok())
                else {
                    return;
                };
                let wrapper = gloo_utils::document()
                    .get_element_by_id(&trigger_id)
                    .and_then(|t| t.parent_element());
                let inside = wrapper.is_some_and(|w| w.contains(Some(&target)));
                let on_backdrop = target
                    .dyn_ref::<web_sys::Element>()
                    .is_some_and(|el| el.matches(".tile-menu-backdrop").unwrap_or(false));
                if !inside && !on_backdrop && *show.peek() {
                    show.set(false);
                }
            },
        );
        let _ = gloo_utils::document().add_event_listener_with_callback_and_bool(
            "pointerdown",
            on_down.as_ref().unchecked_ref(),
            true,
        );
        LIVE_DISMISS_LISTENERS.with(|n| n.set(n.get() + 1));
        std::rc::Rc::new(OutsidePointerDown(on_down))
    });
    rsx! {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: Rect = Rect {
        left: 8.0,
        top: 8.0,
        right: 792.0,
        bottom: 592.0,
    };

    fn trigger(left: f64, top: f64) -> Rect {
        Rect {
            left,
            top,
            right: left + 24.0,
            bottom: top + 24.0,
        }
    }

    #[test]
    fn opens_below_with_its_right_edge_on_the_trigger() {
        let t = trigger(400.0, 100.0);
        let p = place_tile_menu(t, 180.0, 170.0, VIEW);
        assert_eq!(
            p,
            MenuPlacement {
                left: t.right - 180.0,
                top: t.bottom + TRIGGER_GAP_PX,
                max_height: None
            }
        );
    }

    #[test]
    fn flips_above_when_only_the_space_above_fits() {
        let t = trigger(400.0, 500.0);
        let p = place_tile_menu(t, 180.0, 170.0, VIEW);
        assert_eq!(p.top, t.top - TRIGGER_GAP_PX - 170.0);
        assert_eq!(p.max_height, None);
    }

    #[test]
    fn clamps_horizontally_into_the_bounds() {
        let p = place_tile_menu(trigger(20.0, 100.0), 180.0, 170.0, VIEW);
        assert_eq!(p.left, VIEW.left);
        let p = place_tile_menu(trigger(900.0, 100.0), 180.0, 170.0, VIEW);
        assert_eq!(p.left, VIEW.right - 180.0);
    }

    fn assert_clear_of(t: Rect, p: MenuPlacement, menu_h: f64, bounds: Rect) {
        let bottom = p.top + p.max_height.unwrap_or(menu_h);
        assert!(
            bottom <= t.top || p.top >= t.bottom,
            "menu {}..{bottom} covers trigger {}..{}",
            p.top,
            t.top,
            t.bottom
        );
        assert!(p.top >= bounds.top && bottom <= bounds.bottom);
    }

    #[test]
    fn never_covers_the_trigger_when_neither_side_fits() {
        let short = Rect {
            bottom: 300.0,
            ..VIEW
        };
        let below_roomier = trigger(400.0, 100.0);
        let p = place_tile_menu(below_roomier, 180.0, 200.0, short);
        assert_eq!(p.top, below_roomier.bottom + TRIGGER_GAP_PX);
        assert_clear_of(below_roomier, p, 200.0, short);
        let above_roomier = trigger(400.0, 160.0);
        let p = place_tile_menu(above_roomier, 180.0, 200.0, short);
        assert_eq!(p.top, short.top);
        assert_clear_of(above_roomier, p, 200.0, short);
        let tiny = Rect {
            bottom: 50.0,
            ..VIEW
        };
        let sliver_above = trigger(400.0, 18.0);
        let p = place_tile_menu(sliver_above, 180.0, 200.0, tiny);
        assert_eq!(
            p.top,
            sliver_above.bottom + TRIGGER_GAP_PX,
            "4px above cannot hold the menu's chrome, so it goes below the 2px side"
        );
    }

    #[test]
    fn a_menu_taller_than_the_bounds_scrolls_beside_the_trigger() {
        let short = Rect {
            bottom: 150.0,
            ..VIEW
        };
        let t = trigger(400.0, 60.0);
        let p = place_tile_menu(t, 180.0, 200.0, short);
        assert_eq!(p.max_height, Some(short.bottom - t.bottom - TRIGGER_GAP_PX));
        assert_clear_of(t, p, 200.0, short);
    }
}
