// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::components::action_bar_layout::{
    customize_exit_announcement, drag_grab_offset, hold_exceeds_slop, hold_should_fire, idle_step,
    press_pointer, release_pointer, save_action_bar_layout, ActionBarSlot, CustomizeEntry,
    CustomizeExit, IdleStep, HOLD_TO_EDIT_MS,
};
use crate::components::canvas_generator::focus_element_by_id;
use crate::components::window_event_listener::WindowEventListener;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::{JsCast, JsValue};

const SETTLE_CLEAR_MS: u32 = 200;
pub(crate) const NAV_SELECTOR: &str = ".video-controls-container";

/// Resolve the action-bar's actual axis gap (column-gap when horizontal,
/// row-gap when vertical) from computed style instead of hard-coding a value
/// that drifts whenever the CSS changes. Falls back to the design default
/// (1.2rem ≈ 19.2px) when the lookup fails.
fn read_nav_axis_gap_px(nav: &web_sys::Element, is_vertical: bool) -> f64 {
    let prop = if is_vertical { "row-gap" } else { "column-gap" };
    web_sys::window()
        .and_then(|w| w.get_computed_style(nav).ok().flatten())
        .and_then(|s| s.get_property_value(prop).ok())
        .and_then(|s| s.trim().trim_end_matches("px").parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(19.2)
}

pub(crate) fn focus_action_bar_slot(slot: ActionBarSlot) {
    let selector = format!(
        "{NAV_SELECTOR} .action-bar-slot-wrapper[data-slot=\"{}\"] > button.video-control-button",
        slot.slug()
    );
    let Some(button) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(&selector).ok().flatten())
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let opts = web_sys::FocusOptions::new();
    opts.set_prevent_scroll(true);
    let _ = button.focus_with_options(&opts);
}

fn rendered_action_bar_slots(nav: &web_sys::Element) -> Vec<ActionBarSlot> {
    let Ok(nodes) =
        nav.query_selector_all(".action-bar-slot-wrapper[data-slot]:not(.is-overflow-hidden)")
    else {
        return Vec::new();
    };
    (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
        .filter_map(|el| el.get_attribute("data-slot"))
        .filter_map(|slug| ActionBarSlot::from_slug(&slug))
        .collect()
}

fn element_in_action_bar(el: Option<web_sys::Element>) -> bool {
    el.and_then(|el| el.closest(NAV_SELECTOR).ok().flatten())
        .is_some()
}

pub(crate) fn event_in_action_bar(e: &web_sys::Event) -> bool {
    element_in_action_bar(
        e.target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok()),
    )
}

fn focus_is_in_action_bar() -> bool {
    element_in_action_bar(
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element()),
    )
}

fn restore_focus_after_idle_exit() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let lost = match doc.active_element() {
        None => true,
        Some(el) => {
            !el.is_connected()
                || doc
                    .body()
                    .is_some_and(|body| body.is_same_node(Some(el.as_ref())))
        }
    };
    if lost {
        focus_element_by_id("dock-menu-trigger");
    }
}

fn vibrate_briefly() {
    let Some(navigator) = web_sys::window().map(|w| w.navigator()) else {
        return;
    };
    if js_sys::Reflect::has(&navigator, &JsValue::from_str("vibrate")).unwrap_or(false) {
        let _ = navigator.vibrate_with_duration(15);
    }
}

fn clear_marker(el: &web_sys::Element) {
    let _ = el.remove_attribute("data-hold");
}

#[derive(Clone)]
pub(crate) struct SlotDragHandles {
    pub(crate) dragging_slot: Signal<Option<ActionBarSlot>>,
    pub(crate) drag_pointer_x: Signal<f64>,
    pub(crate) drag_pointer_y: Signal<f64>,
    pub(crate) drag_insertion_idx: Signal<Option<usize>>,
    pub(crate) drag_started: Signal<bool>,
    pub(crate) drag_orig_layout: Signal<Vec<ActionBarSlot>>,
    pub(crate) drag_slot_size: Rc<Cell<f64>>,
    pub(crate) drag_grab_dx: Rc<Cell<f64>>,
    pub(crate) drag_grab_dy: Rc<Cell<f64>>,
    pub(crate) drag_pointer_id: Rc<Cell<i32>>,
    pub(crate) drag_start_x: Rc<Cell<f64>>,
    pub(crate) drag_start_y: Rc<Cell<f64>>,
    pub(crate) drag_nav_left: Rc<Cell<f64>>,
    pub(crate) drag_nav_top: Rc<Cell<f64>>,
}

#[derive(Clone, Copy)]
pub(crate) struct SlotDragStart {
    pub(crate) slot: ActionBarSlot,
    pub(crate) src_idx: usize,
    pub(crate) pointer_id: i32,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) grab_at: (f64, f64),
    pub(crate) is_vertical: bool,
}

pub(crate) fn begin_slot_drag(
    h: &SlotDragHandles,
    start: SlotDragStart,
    layout: Vec<ActionBarSlot>,
    wrapper: Option<web_sys::Element>,
) {
    let (cx, cy) = (start.x, start.y);
    h.drag_start_x.set(cx);
    h.drag_start_y.set(cy);
    let mut drag_started = h.drag_started;
    drag_started.set(false);
    h.drag_pointer_id.set(start.pointer_id);
    let mut drag_orig_layout = h.drag_orig_layout;
    drag_orig_layout.set(layout);
    let mut dragging_slot = h.dragging_slot;
    dragging_slot.set(Some(start.slot));
    let mut drag_pointer_x = h.drag_pointer_x;
    drag_pointer_x.set(cx);
    let mut drag_pointer_y = h.drag_pointer_y;
    drag_pointer_y.set(cy);
    let mut drag_insertion_idx = h.drag_insertion_idx;
    drag_insertion_idx.set(Some(start.src_idx));
    let Some(el) = wrapper else {
        return;
    };
    let rect = el.get_bounding_client_rect();
    let (grab_dx, grab_dy) = drag_grab_offset(
        start.grab_at,
        (rect.left(), rect.top()),
        (rect.width(), rect.height()),
    );
    h.drag_grab_dx.set(grab_dx);
    h.drag_grab_dy.set(grab_dy);
    let size = if start.is_vertical {
        rect.height()
    } else {
        rect.width()
    };
    if let Ok(Some(nav)) = el.closest(NAV_SELECTOR) {
        let nrect = nav.get_bounding_client_rect();
        h.drag_nav_left.set(nrect.left());
        h.drag_nav_top.set(nrect.top());
        let gap = read_nav_axis_gap_px(&nav, start.is_vertical);
        h.drag_slot_size.set(size + gap);
        let _ = nav.set_pointer_capture(start.pointer_id);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CustomizeCtl {
    pub(crate) customize_mode: Signal<bool>,
    pub(crate) entry: Signal<CustomizeEntry>,
    pub(crate) exit_announce: Signal<&'static str>,
    pub(crate) action_bar_slots: Signal<Vec<ActionBarSlot>>,
    pub(crate) action_bar_hidden: Signal<Vec<ActionBarSlot>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ActionBarPopovers {
    pub(crate) density_open: Signal<bool>,
    pub(crate) reactions_open: Signal<bool>,
    pub(crate) mock_peers_open: Signal<bool>,
    pub(crate) share_menu_open: Signal<bool>,
    pub(crate) dock_menu_open: Signal<bool>,
    pub(crate) meeting_timer_open: Signal<bool>,
    pub(crate) recents_open: Signal<bool>,
}

impl ActionBarPopovers {
    pub(crate) fn transient(self) -> [Signal<bool>; 6] {
        [
            self.density_open,
            self.reactions_open,
            self.mock_peers_open,
            self.share_menu_open,
            self.dock_menu_open,
            self.recents_open,
        ]
    }

    fn close_all(self) {
        for mut open in [
            self.density_open,
            self.reactions_open,
            self.mock_peers_open,
            self.share_menu_open,
            self.dock_menu_open,
            self.meeting_timer_open,
            self.recents_open,
        ] {
            if *open.peek() {
                open.set(false);
            }
        }
    }
}

impl CustomizeCtl {
    pub(crate) fn enter(mut self, entry: CustomizeEntry, popovers: ActionBarPopovers) {
        self.entry.set(entry);
        self.customize_mode.set(true);
        popovers.close_all();
    }

    pub(crate) fn exit(mut self, reason: CustomizeExit) -> bool {
        if !*self.customize_mode.peek() {
            return false;
        }
        let focus_was_in_bar = focus_is_in_action_bar();
        let announce = customize_exit_announcement(reason);
        if *self.exit_announce.peek() != announce {
            self.exit_announce.set(announce);
        }
        self.customize_mode.set(false);
        save_action_bar_layout(
            &self.action_bar_slots.peek(),
            &self.action_bar_hidden.peek(),
        );
        match reason {
            CustomizeExit::Done | CustomizeExit::Escape => {
                Timeout::new(0, || focus_element_by_id("dock-menu-trigger")).forget();
            }
            CustomizeExit::Idle if focus_was_in_bar => {
                Timeout::new(0, restore_focus_after_idle_exit).forget();
            }
            CustomizeExit::Idle | CustomizeExit::BandTooNarrow => {}
        }
        true
    }
}

pub(crate) struct PendingHold {
    pub(crate) slot: ActionBarSlot,
    pub(crate) pointer_id: i32,
    pub(crate) down: (f64, f64),
    pub(crate) last: (f64, f64),
    pub(crate) touch: bool,
    pub(crate) wrapper: web_sys::Element,
}

pub(crate) struct HoldContinuation {
    pub(crate) slot: ActionBarSlot,
    pub(crate) origin: (f64, f64),
    pub(crate) wrapper: web_sys::Element,
}

struct ContinuationState {
    pointer_id: i32,
    hold: HoldContinuation,
}

#[derive(Clone, Copy)]
pub(crate) struct HoldContext {
    pub(crate) ctl: CustomizeCtl,
    pub(crate) popovers: ActionBarPopovers,
    pub(crate) overflowed_slots: Signal<Vec<ActionBarSlot>>,
    pub(crate) dock_wrapper_hidden: Signal<bool>,
}

#[derive(Default)]
pub(crate) struct ActionBarGesture {
    pending: RefCell<Option<PendingHold>>,
    hold_timer: RefCell<Option<Timeout>>,
    continuation: RefCell<Option<ContinuationState>>,
    settle: RefCell<Option<(web_sys::Element, Timeout)>>,
    idle_timed: Cell<bool>,
    last_activity: Cell<f64>,
    pointers_down_in_bar: RefCell<Vec<i32>>,
    idle_task: Cell<Option<dioxus_core::Task>>,
    hangup_down_at: Cell<Option<f64>>,
}

impl ActionBarGesture {
    pub(crate) fn cancel_hold(&self) {
        let pending = self.pending.borrow_mut().take();
        drop(self.hold_timer.borrow_mut().take());
        if let Some(hold) = pending {
            clear_marker(&hold.wrapper);
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.borrow().is_some()
    }

    pub(crate) fn pending_slot(&self) -> Option<ActionBarSlot> {
        self.pending.borrow().as_ref().map(|h| h.slot)
    }

    pub(crate) fn pending_pointer(&self) -> Option<i32> {
        self.pending.borrow().as_ref().map(|h| h.pointer_id)
    }

    pub(crate) fn track_hold_move(&self, pointer_id: i32, at: (f64, f64)) {
        let exceeded = {
            let mut pending = self.pending.borrow_mut();
            let Some(hold) = pending.as_mut().filter(|h| h.pointer_id == pointer_id) else {
                return;
            };
            hold.last = at;
            hold_exceeds_slop(hold.down, at)
        };
        if exceeded {
            self.cancel_hold();
        }
    }

    pub(crate) fn take_continuation(&self, pointer_id: i32) -> Option<HoldContinuation> {
        let mut continuation = self.continuation.borrow_mut();
        if continuation
            .as_ref()
            .is_some_and(|c| c.pointer_id == pointer_id)
        {
            continuation.take().map(|c| c.hold)
        } else {
            None
        }
    }

    pub(crate) fn note_activity(&self) {
        self.last_activity.set(js_sys::Date::now());
    }

    pub(crate) fn pointer_held_in_bar(&self) -> bool {
        !self.pointers_down_in_bar.borrow().is_empty()
    }

    pub(crate) fn idle_is_timed(&self) -> bool {
        self.idle_timed.get()
    }

    pub(crate) fn start_idle_clock(&self) {
        self.idle_timed.set(true);
    }

    pub(crate) fn set_idle_task(&self, task: dioxus_core::Task) {
        self.idle_task.set(Some(task));
    }

    pub(crate) fn stop_idle_clock(&self) {
        self.idle_timed.set(false);
        if let Some(task) = self.idle_task.take() {
            task.cancel();
        }
    }

    pub(crate) fn mark_hangup_press(&self) {
        self.hangup_down_at.set(Some(js_sys::Date::now()));
    }

    pub(crate) fn take_hangup_press(&self) -> Option<f64> {
        self.hangup_down_at.take()
    }

    fn clear_settle(&self) {
        if let Some((wrapper, _timer)) = self.settle.borrow_mut().take() {
            clear_marker(&wrapper);
        }
    }

    pub(crate) fn reset_after_exit(&self) {
        self.continuation.borrow_mut().take();
        self.pointers_down_in_bar.borrow_mut().clear();
        self.clear_settle();
    }

    fn abandon_pointers(&self) {
        self.cancel_hold();
        self.continuation.borrow_mut().take();
        self.pointers_down_in_bar.borrow_mut().clear();
    }

    fn press_in_bar(&self, pointer_id: i32) {
        press_pointer(&mut self.pointers_down_in_bar.borrow_mut(), pointer_id);
        self.note_activity();
    }

    fn end_pointer(&self, pointer_id: i32) {
        if self.pending_pointer() == Some(pointer_id) {
            self.cancel_hold();
        }
        let _ = self.take_continuation(pointer_id);
        let released_in_bar =
            release_pointer(&mut self.pointers_down_in_bar.borrow_mut(), pointer_id);
        if released_in_bar {
            self.note_activity();
        }
    }
}

pub(crate) async fn run_idle_clock(
    gesture: Rc<ActionBarGesture>,
    ctl: CustomizeCtl,
    on_idle_exit: impl FnOnce(),
) {
    while gesture.idle_timed.get() {
        match idle_step(
            js_sys::Date::now(),
            gesture.last_activity.get(),
            gesture.pointer_held_in_bar(),
        ) {
            IdleStep::Exit => {
                gesture.idle_task.set(None);
                if ctl.exit(CustomizeExit::Idle) {
                    on_idle_exit();
                }
                return;
            }
            IdleStep::Wait(ms) => gloo_timers::future::TimeoutFuture::new(ms).await,
        }
    }
    gesture.idle_task.set(None);
}

pub(crate) fn arm_action_bar_hold(
    gesture: &Rc<ActionBarGesture>,
    hold: PendingHold,
    cx: HoldContext,
) {
    gesture.cancel_hold();
    gesture.clear_settle();
    let _ = hold.wrapper.set_attribute("data-hold", "pending");
    *gesture.pending.borrow_mut() = Some(hold);
    let weak = Rc::downgrade(gesture);
    let timer = Timeout::new(HOLD_TO_EDIT_MS, move || {
        if let Some(gesture) = weak.upgrade() {
            fire_action_bar_hold(&gesture, cx);
        }
    });
    *gesture.hold_timer.borrow_mut() = Some(timer);
}

fn fire_action_bar_hold(gesture: &ActionBarGesture, cx: HoldContext) {
    let pending = gesture.pending.borrow_mut().take();
    let Some(hold) = pending else {
        return;
    };
    let (Ok(editing), Ok(overflowed), Ok(band_too_narrow)) = (
        cx.ctl.customize_mode.try_peek().map(|v| *v),
        cx.overflowed_slots.try_peek().map(|v| v.clone()),
        cx.dock_wrapper_hidden.try_peek().map(|v| *v),
    ) else {
        return;
    };
    let rendered = hold
        .wrapper
        .closest(NAV_SELECTOR)
        .ok()
        .flatten()
        .map(|nav| rendered_action_bar_slots(&nav))
        .unwrap_or_default();
    if !hold_should_fire(editing, !band_too_narrow, hold.slot, &rendered, &overflowed) {
        clear_marker(&hold.wrapper);
        return;
    }
    let _ = hold.wrapper.set_attribute("data-hold", "fired");
    let settled = hold.wrapper.clone();
    let settle_timer = Timeout::new(SETTLE_CLEAR_MS, move || clear_marker(&settled));
    *gesture.settle.borrow_mut() = Some((hold.wrapper.clone(), settle_timer));
    gesture.press_in_bar(hold.pointer_id);
    if hold.touch {
        vibrate_briefly();
    }
    *gesture.continuation.borrow_mut() = Some(ContinuationState {
        pointer_id: hold.pointer_id,
        hold: HoldContinuation {
            slot: hold.slot,
            origin: hold.last,
            wrapper: hold.wrapper,
        },
    });
    cx.ctl.enter(CustomizeEntry::Hold(hold.slot), cx.popovers);
}

pub(crate) fn install_action_bar_gesture_listeners(
    gesture: Rc<ActionBarGesture>,
    customize_mode: Signal<bool>,
) -> Vec<WindowEventListener> {
    let on_down = gesture.clone();
    let on_blur = gesture.clone();
    let on_hidden = gesture.clone();
    let mut listeners = vec![
        WindowEventListener::with_event("pointerdown", true, move |e| {
            on_down.hangup_down_at.set(None);
            if on_down.has_pending() {
                on_down.cancel_hold();
            }
            let editing = customize_mode.try_peek().map(|v| *v).unwrap_or(false);
            if let (true, Some(pe)) = (editing, e.dyn_ref::<web_sys::PointerEvent>()) {
                if event_in_action_bar(&e) {
                    on_down.press_in_bar(pe.pointer_id());
                }
            }
        }),
        WindowEventListener::with_event("blur", false, move |_| on_blur.abandon_pointers()),
        WindowEventListener::with_event("visibilitychange", false, move |_| {
            if web_sys::window()
                .and_then(|w| w.document())
                .is_some_and(|d| d.hidden())
            {
                on_hidden.abandon_pointers();
            }
        }),
    ];
    for event in ["pointerup", "pointercancel"] {
        let on_end = gesture.clone();
        listeners.push(WindowEventListener::with_event(event, true, move |e| {
            if let Some(pe) = e.dyn_ref::<web_sys::PointerEvent>() {
                on_end.end_pointer(pe.pointer_id());
            }
        }));
    }
    listeners
}
