// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::components::action_bar_gesture::{event_in_action_bar, NAV_SELECTOR};
use crate::components::attendants::try_set;
use crate::components::window_event_listener::WindowEventListener;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::JsCast;

const HOVER_OPEN_DELAY_MS: u32 = 300;
const HOVER_CLOSE_DELAY_MS: u32 = 400;
const HOVER_OPEN_ATTR: &str = "data-hover-open";
const COARSE_HANDLE_LENGTH_PX: f64 = 96.0;
const HOVER_SNAP_CLICK_GUARD_MS: f64 = 400.0;
const BAR_HELD_OPEN_SELECTOR: &str = ".controls-expanded, .customize-mode, :has(:focus-visible)";
const CLIPPED_SLOT_SELECTOR: &str = ".video-controls-container .slot-shrinkable";

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BarExpansion {
    pub(crate) autoshrink: bool,
    pub(crate) touch_latch: bool,
    pub(crate) customize_mode: bool,
    pub(crate) share_pinned: bool,
    pub(crate) transient_popover_open: bool,
    pub(crate) meeting_timer_open: bool,
    pub(crate) is_host: bool,
}

pub(crate) fn bar_popover_open(
    transient_popover_open: bool,
    meeting_timer_open: bool,
    is_host: bool,
) -> bool {
    transient_popover_open || (meeting_timer_open && is_host)
}

pub(crate) fn bar_is_expanded(state: BarExpansion) -> bool {
    !state.autoshrink
        || state.touch_latch
        || state.customize_mode
        || state.share_pinned
        || bar_popover_open(
            state.transient_popover_open,
            state.meeting_timer_open,
            state.is_host,
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BarLayout {
    pub(crate) expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarShift {
    None,
    Shrank,
}

fn bar_shift(prior: BarLayout, next: BarLayout) -> BarShift {
    if prior.expanded && !next.expanded {
        BarShift::Shrank
    } else {
        BarShift::None
    }
}

fn pointer_uses_hover(pointer_type: &str) -> bool {
    !matches!(pointer_type, "touch" | "pen")
}

pub(crate) fn any_pointer_is_coarse() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(any-pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches())
}

/// Room the overflow budget leaves for the handle, which only stays on an
/// expanded bar under a coarse pointer.
pub(crate) fn expand_handle_reserve(autoshrink: bool, coarse_pointer: bool, rem_px: f64) -> f64 {
    if autoshrink && coarse_pointer {
        COARSE_HANDLE_LENGTH_PX + 1.2 * rem_px
    } else {
        0.0
    }
}

pub(crate) fn handle_click_toggles_latch(pointer_type: Option<&str>) -> bool {
    pointer_type != Some("mouse")
}

pub(crate) fn click_pointer_type(e: &web_sys::Event) -> Option<String> {
    e.dyn_ref::<web_sys::PointerEvent>()
        .map(web_sys::PointerEvent::pointer_type)
}

fn click_is_guarded(snapped_at: Option<f64>, pressed_at: Option<f64>, detail: i32) -> bool {
    let (Some(snapped), Some(pressed)) = (snapped_at, pressed_at) else {
        return false;
    };
    detail != 0 && (0.0..HOVER_SNAP_CLICK_GUARD_MS).contains(&(pressed - snapped))
}

fn bar_nav() -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(NAV_SELECTOR).ok().flatten())
}

fn nav_is_collapsed(nav: &web_sys::Element) -> bool {
    !nav.has_attribute(HOVER_OPEN_ATTR) && !nav.matches(BAR_HELD_OPEN_SELECTOR).unwrap_or(true)
}

fn clipped_focus_should_blur(window_has_focus: bool, clipped: bool, collapsed: bool) -> bool {
    window_has_focus && clipped && collapsed
}

pub(crate) fn blur_clipped_focus() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(active) = doc.active_element() else {
        return;
    };
    let clipped = active
        .closest(CLIPPED_SLOT_SELECTOR)
        .ok()
        .flatten()
        .is_some();
    let collapsed = bar_nav().is_some_and(|nav| nav_is_collapsed(&nav));
    if clipped_focus_should_blur(doc.has_focus().unwrap_or(false), clipped, collapsed) {
        if let Some(html) = active.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.blur();
        }
    }
}

fn set_hover_open(open: bool, snapped_at: &Cell<Option<f64>>) {
    let Some(nav) = bar_nav() else {
        return;
    };
    if nav.has_attribute(HOVER_OPEN_ATTR) == open {
        return;
    }
    let held_open = nav.matches(BAR_HELD_OPEN_SELECTOR).unwrap_or(true);
    let _ = if open {
        nav.set_attribute(HOVER_OPEN_ATTR, "")
    } else {
        nav.remove_attribute(HOVER_OPEN_ATTR)
    };
    if held_open {
        return;
    }
    snapped_at.set(Some(js_sys::Date::now()));
    if !open {
        blur_clipped_focus();
    }
}

fn nav_is_bare() -> bool {
    bar_nav().is_some_and(|nav| nav.class_list().contains("shrink-empty"))
}

#[derive(Default)]
pub(crate) struct BarHoverIntent {
    open_timer: RefCell<Option<Timeout>>,
    close_timer: RefCell<Option<Timeout>>,
    // Chromium's enter and leave do not pair when the DOM changes under the
    // pointer: a repeat enter arrives with no leave, or the leave never comes.
    inside: Cell<bool>,
    snapped_at: Rc<Cell<Option<f64>>>,
    pressed_at: Cell<Option<f64>>,
    layout: Cell<Option<BarLayout>>,
    blur_timer: RefCell<Option<Timeout>>,
}

impl BarHoverIntent {
    pub(crate) fn enter(&self, pointer_type: &str) {
        if !pointer_uses_hover(pointer_type) {
            return;
        }
        self.close_timer.borrow_mut().take();
        if self.inside.replace(true) {
            return;
        }
        if nav_is_bare() {
            set_hover_open(true, &self.snapped_at);
            return;
        }
        let snapped_at = self.snapped_at.clone();
        *self.open_timer.borrow_mut() = Some(Timeout::new(HOVER_OPEN_DELAY_MS, move || {
            set_hover_open(true, &snapped_at)
        }));
    }

    pub(crate) fn leave(&self, pointer_type: &str) {
        if pointer_uses_hover(pointer_type) {
            self.pointer_left();
        }
    }

    pub(crate) fn note_pointer_move(&self, e: &web_sys::Event) {
        if self.inside.get() && !event_in_action_bar(e) {
            self.pointer_left();
        }
    }

    fn pointer_left(&self) {
        self.inside.set(false);
        self.open_timer.borrow_mut().take();
        let snapped_at = self.snapped_at.clone();
        *self.close_timer.borrow_mut() = Some(Timeout::new(HOVER_CLOSE_DELAY_MS, move || {
            set_hover_open(false, &snapped_at)
        }));
    }

    fn cancel_pending_open(&self) {
        self.open_timer.borrow_mut().take();
    }

    pub(crate) fn open_now(&self) {
        set_hover_open(true, &self.snapped_at);
    }

    pub(crate) fn note_layout(&self, next: BarLayout) {
        let Some(prior) = self.layout.replace(Some(next)) else {
            return;
        };
        let shift = bar_shift(prior, next);
        if shift == BarShift::None || !bar_nav().is_some_and(|nav| nav_is_collapsed(&nav)) {
            return;
        }
        self.snapped_at.set(Some(js_sys::Date::now()));
        *self.blur_timer.borrow_mut() = Some(Timeout::new(0, blur_clipped_focus));
    }

    fn dismiss(&self) {
        self.open_timer.borrow_mut().take();
        self.close_timer.borrow_mut().take();
        set_hover_open(false, &self.snapped_at);
    }
}

pub(crate) fn install_action_bar_shrink(
    hover: Rc<BarHoverIntent>,
    touch_latch: Signal<bool>,
    customize_mode: Signal<bool>,
    popover_open: impl Fn() -> bool + 'static,
) -> [WindowEventListener; 3] {
    let on_down = hover.clone();
    let on_click = hover.clone();
    [
        WindowEventListener::with_event("pointerdown", true, move |e| {
            if event_in_action_bar(&e) {
                on_down.pressed_at.set(Some(js_sys::Date::now()));
                on_down.cancel_pending_open();
            } else if touch_latch.try_peek().is_ok_and(|on| *on) {
                try_set(touch_latch, false);
            }
        }),
        WindowEventListener::with_event("keydown", true, move |e| {
            let escape = e
                .dyn_ref::<web_sys::KeyboardEvent>()
                .is_some_and(|k| k.key() == "Escape");
            let editing = customize_mode.try_peek().is_ok_and(|on| *on);
            if escape && !editing && !popover_open() {
                hover.dismiss();
            }
        }),
        WindowEventListener::with_event("click", true, move |e| {
            let detail = e.dyn_ref::<web_sys::MouseEvent>().map_or(0, |m| m.detail());
            if click_is_guarded(on_click.snapped_at.get(), on_click.pressed_at.get(), detail)
                && event_in_action_bar(&e)
            {
                e.stop_propagation();
                e.prevent_default();
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_with_shrinking_off_is_always_expanded() {
        assert!(bar_is_expanded(BarExpansion::default()));
    }

    #[test]
    fn a_shrinking_bar_collapses_only_with_nothing_holding_it_open() {
        let idle = BarExpansion {
            autoshrink: true,
            is_host: true,
            ..Default::default()
        };
        assert!(!bar_is_expanded(idle));
        for (name, held) in [
            (
                "touch latch",
                BarExpansion {
                    touch_latch: true,
                    ..idle
                },
            ),
            (
                "customize mode",
                BarExpansion {
                    customize_mode: true,
                    ..idle
                },
            ),
            (
                "share pin",
                BarExpansion {
                    share_pinned: true,
                    ..idle
                },
            ),
            (
                "popover",
                BarExpansion {
                    transient_popover_open: true,
                    ..idle
                },
            ),
            (
                "host's timer popover",
                BarExpansion {
                    meeting_timer_open: true,
                    ..idle
                },
            ),
        ] {
            assert!(bar_is_expanded(held), "{name}");
        }
    }

    #[test]
    fn a_demoted_hosts_stale_timer_flag_does_not_hold_the_bar_open() {
        assert!(!bar_is_expanded(BarExpansion {
            autoshrink: true,
            meeting_timer_open: true,
            is_host: false,
            ..Default::default()
        }));
    }

    #[test]
    fn only_a_real_move_of_the_bar_is_a_shift() {
        let layout = |expanded| BarLayout { expanded };
        for (why, prior, next, shift) in [
            (
                "expanded falls",
                layout(true),
                layout(false),
                BarShift::Shrank,
            ),
            (
                "expanded rises: a kept button's own click",
                layout(false),
                layout(true),
                BarShift::None,
            ),
            (
                "expanded and unchanged",
                layout(true),
                layout(true),
                BarShift::None,
            ),
            (
                "collapsed and unchanged",
                layout(false),
                layout(false),
                BarShift::None,
            ),
        ] {
            assert_eq!(bar_shift(prior, next), shift, "{why}");
        }
    }

    #[test]
    fn the_blur_needs_a_focused_window_a_clipped_control_and_a_collapsed_bar() {
        assert!(clipped_focus_should_blur(true, true, true));
        assert!(
            !clipped_focus_should_blur(false, true, true),
            "the window is in the background"
        );
        assert!(!clipped_focus_should_blur(true, false, true));
        assert!(!clipped_focus_should_blur(true, true, false));
    }

    #[test]
    fn the_blur_asks_the_document_whether_the_window_has_focus() {
        let flat: String = include_str!("action_bar_shrink.rs")
            .split_whitespace()
            .collect();
        assert!(flat.contains(concat!(
            "clipped_focus_should_blur(",
            "doc.has_focus().unwrap_or(false),clipped,collapsed)"
        )));
    }

    #[test]
    fn only_a_mouse_opens_the_bar_on_hover() {
        assert!(pointer_uses_hover("mouse"));
        assert!(!pointer_uses_hover("touch"));
        assert!(!pointer_uses_hover("pen"));
    }

    #[test]
    fn the_overflow_budget_reserves_the_handle_only_when_it_stays_on_screen() {
        assert_eq!(expand_handle_reserve(false, true, 16.0), 0.0);
        assert_eq!(expand_handle_reserve(true, false, 16.0), 0.0);
        assert_eq!(
            expand_handle_reserve(true, true, 16.0),
            COARSE_HANDLE_LENGTH_PX + 1.2 * 16.0
        );
    }

    fn global_css() -> String {
        include_str!("../../static/global.css")
            .split_whitespace()
            .collect()
    }

    fn rule_declares(css: &str, selector: &str, declaration: &str) -> bool {
        css.split_once(selector)
            .and_then(|(_, rest)| rest.split('}').next())
            .is_some_and(|body| body.split(';').any(|d| d == declaration))
    }

    #[test]
    fn the_reserved_length_matches_the_coarse_handle_css() {
        let flat = global_css();
        let (_, coarse) = flat
            .split_once("@media(any-pointer:coarse){")
            .expect("the coarse handle media query");
        assert!(
            rule_declares(
                coarse,
                ".action-bar-expand-handle{",
                &format!("width:{COARSE_HANDLE_LENGTH_PX}px")
            ),
            "the bottom dock's handle length"
        );
        assert!(
            rule_declares(
                coarse,
                ".dock-left.action-bar-expand-handle,.dock-right.action-bar-expand-handle{",
                &format!("height:{COARSE_HANDLE_LENGTH_PX}px")
            ),
            "the side docks' handle length"
        );
    }

    #[test]
    fn the_pause_rule_names_its_animators() {
        let flat = global_css();
        let (before, _) = flat
            .split_once("{animation-play-state:paused!important")
            .expect("the pause rule");
        let selectors = before.rsplit('}').next().expect("its selectors");
        for animator in [
            ".chat-unread-badge",
            ".record-active",
            ".captions-button[aria-busy=\"true\"]svg",
        ] {
            assert!(
                selectors.contains(&format!(".slot-shrinkable{animator}")),
                "{animator}"
            );
        }
        assert!(
            !selectors.contains('*'),
            "a universal selector restyles every control on each flip"
        );
    }

    #[test]
    fn the_handle_grip_shows_in_forced_colors() {
        assert!(rule_declares(
            &global_css(),
            "@media(forced-colors:active){.action-bar-expand-handle::before{",
            "background-color:CanvasText"
        ));
    }

    #[test]
    fn only_a_mouse_click_on_the_handle_skips_the_latch() {
        assert!(!handle_click_toggles_latch(Some("mouse")));
        assert!(handle_click_toggles_latch(Some("touch")));
        assert!(handle_click_toggles_latch(Some("pen")));
        assert!(handle_click_toggles_latch(Some("")));
        assert!(handle_click_toggles_latch(None));
    }

    #[test]
    fn a_click_is_guarded_when_pressed_within_400_ms_after_a_snap() {
        let snap = Some(1_000.0);
        assert!(click_is_guarded(snap, Some(1_000.0), 1));
        assert!(click_is_guarded(snap, Some(1_399.0), 1));
        assert!(!click_is_guarded(snap, Some(1_400.0), 1));
        assert!(
            !click_is_guarded(snap, Some(999.0), 1),
            "a press begun before the snap is a legitimate click"
        );
        assert!(
            !click_is_guarded(snap, Some(1_100.0), 0),
            "keyboard activation has detail 0"
        );
        assert!(!click_is_guarded(None, Some(1_100.0), 1));
        assert!(!click_is_guarded(snap, None, 1));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen_test::wasm_bindgen_test;

    const SETTLE_MS: u32 = 150;

    type ProbeSignals = (Signal<bool>, Signal<bool>);
    thread_local! {
        static PROBE: RefCell<Option<ProbeSignals>> = const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn Probe() -> Element {
        let touch_latch = use_signal(|| false);
        let customize_mode = use_signal(|| false);
        use_hook(move || PROBE.with(|p| *p.borrow_mut() = Some((touch_latch, customize_mode))));
        rsx! { div {} }
    }

    fn probe() -> (VirtualDom, Signal<bool>, Signal<bool>) {
        let mut vdom = VirtualDom::new(Probe);
        vdom.rebuild_in_place();
        let (touch_latch, customize_mode) = PROBE.with(|p| p.borrow().expect("probe signals"));
        (vdom, touch_latch, customize_mode)
    }

    fn mount_nav() -> web_sys::Element {
        let doc = gloo_utils::document();
        let nav = doc.create_element("nav").unwrap();
        nav.set_class_name("video-controls-container");
        let inner = doc.create_element("button").unwrap();
        nav.append_child(&inner).unwrap();
        doc.body().unwrap().prepend_with_node_1(&nav).unwrap();
        nav
    }

    fn is_open(nav: &web_sys::Element) -> bool {
        nav.has_attribute(HOVER_OPEN_ATTR)
    }

    fn event_on(name: &str, target: &web_sys::Element) {
        let init = web_sys::EventInit::new();
        init.set_bubbles(true);
        let event = web_sys::Event::new_with_event_init_dict(name, &init).unwrap();
        target.dispatch_event(&event).unwrap();
    }

    fn pointer_down_on(target: &web_sys::Element) {
        event_on("pointerdown", target);
    }

    fn press_escape() {
        js_sys::eval(
            "document.body.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}))",
        )
        .unwrap();
    }

    const BAR_FIXTURE: &str = "\
        <nav id='bar' class='video-controls-container dock-bottom auto-shrink' style='transition:none'>\
          <button id='handle' class='action-bar-expand-handle'></button>\
          <div id='kept' class='action-bar-slot-wrapper'><button style='width:40px;height:40px'></button></div>\
          <div id='slot' class='action-bar-slot-wrapper slot-shrinkable'>\
            <button id='slot-button' class='video-control-button record-active' style='width:40px;height:40px'></button>\
            <button class='video-control-button' style='width:40px;height:40px'><div id='chat-badge' class='chat-unread-badge'></div></button>\
            <button class='video-control-button captions-button' aria-busy='true' style='width:40px;height:40px'><svg id='captions-icon'></svg></button>\
          </div>\
          <div id='overflow-on' class='action-bar-overflow-wrapper slot-shrinkable' \
               style='position:relative;display:inline-flex'><button style='width:40px;height:40px'></button></div>\
          <div id='overflow-off' class='action-bar-overflow-wrapper slot-shrinkable' \
               style='position:relative;display:none'><button style='width:40px;height:40px'></button></div>\
        </nav>";

    const ANIMATORS: [&str; 3] = ["slot-button", "chat-badge", "captions-icon"];

    struct BarFixture {
        host: web_sys::Element,
        sheets: Vec<web_sys::Element>,
    }

    impl BarFixture {
        fn mount() -> Self {
            let doc = gloo_utils::document();
            let sheets = [
                include_str!("../../static/global.css"),
                include_str!("../../static/style.css"),
            ]
            .map(|css| {
                let style = doc.create_element("style").unwrap();
                style.set_text_content(Some(css));
                doc.head().unwrap().append_child(&style).unwrap();
                style
            })
            .to_vec();
            let host = doc.create_element("div").unwrap();
            host.set_inner_html(BAR_FIXTURE);
            doc.body().unwrap().prepend_with_node_1(&host).unwrap();
            Self { host, sheets }
        }

        fn el(&self, id: &str) -> web_sys::HtmlElement {
            self.host
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap_or_else(|| panic!("no #{id}"))
                .unchecked_into()
        }

        fn width(&self, id: &str) -> f64 {
            self.el(id).get_bounding_client_rect().width()
        }

        fn computed(&self, id: &str, property: &str) -> String {
            gloo_utils::window()
                .get_computed_style(&self.el(id))
                .unwrap()
                .unwrap()
                .get_property_value(property)
                .unwrap()
        }

        fn assert_collapsed(&self, why: &str) {
            for id in ["slot", "overflow-on"] {
                assert_eq!(self.width(id), 1.0, "{why}: #{id}");
                assert_eq!(self.computed(id, "position"), "absolute", "{why}: #{id}");
            }
            assert_eq!(self.computed("slot", "clip-path"), "inset(50%)", "{why}");
            for id in ANIMATORS {
                assert_eq!(
                    self.computed(id, "animation-play-state"),
                    "paused",
                    "{why}: #{id} does not keep animating out of sight"
                );
            }
            assert!(self.width("kept") >= 40.0, "{why}: a kept button stays");
        }

        fn assert_revealed(&self, why: &str) {
            for id in ["slot", "overflow-on"] {
                assert!(self.width(id) >= 40.0, "{why}: #{id}");
            }
            for id in ANIMATORS {
                assert_eq!(
                    self.computed(id, "animation-play-state"),
                    "running",
                    "{why}: #{id}"
                );
            }
            assert_eq!(
                self.computed("overflow-off", "display"),
                "none",
                "{why}: an inline display:none still wins"
            );
        }

        fn remove(self) {
            self.host.remove();
            self.sheets.iter().for_each(web_sys::Element::remove);
        }
    }

    #[wasm_bindgen_test]
    fn each_expand_state_reveals_the_clipped_slots_and_keeps_inline_display() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        bar.assert_collapsed("no expand state");

        for class in ["controls-expanded", "customize-mode"] {
            nav.class_list().add_1(class).unwrap();
            bar.assert_revealed(class);
            nav.class_list().remove_1(class).unwrap();
            bar.assert_collapsed("the class went away");
        }
        nav.set_attribute(HOVER_OPEN_ATTR, "").unwrap();
        bar.assert_revealed(HOVER_OPEN_ATTR);
        nav.remove_attribute(HOVER_OPEN_ATTR).unwrap();
        bar.assert_collapsed("the attribute went away");
        bar.remove();
    }

    #[wasm_bindgen_test]
    fn a_clipped_control_takes_focus_and_the_bar_opens_around_it() {
        let bar = BarFixture::mount();
        let button = bar.el("slot-button");
        bar.assert_collapsed("before focus");

        button.focus().unwrap();
        assert_eq!(
            gloo_utils::document().active_element().map(|el| el.id()),
            Some("slot-button".to_string()),
            "a focus restore aimed at a clipped control must land"
        );
        assert!(button.matches(":focus-visible").unwrap());
        bar.assert_revealed("keyboard focus inside");

        button.blur().unwrap();
        bar.assert_collapsed("focus left");
        bar.remove();
    }

    #[wasm_bindgen_test]
    fn tab_into_a_faded_bar_restores_opacity_and_position() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        nav.class_list().add_1("controls-hidden").unwrap();
        assert_eq!(bar.computed("bar", "opacity"), "0.1");

        bar.el("slot-button").focus().unwrap();
        assert_eq!(bar.computed("bar", "opacity"), "1");
        assert!(
            bar.computed("bar", "transform").ends_with(", 0)"),
            "slid back to its resting place, got {}",
            bar.computed("bar", "transform")
        );
        bar.el("slot-button").blur().unwrap();
        bar.remove();
    }

    #[wasm_bindgen_test]
    fn an_empty_bar_collapses_to_the_handle() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        if !any_pointer_is_coarse() {
            assert_eq!(bar.computed("handle", "display"), "none");
        }

        nav.class_list().add_1("shrink-empty").unwrap();
        assert_eq!(bar.computed("handle", "display"), "flex");
        assert_eq!(bar.computed("bar", "padding-left"), "0px");
        assert_eq!(
            bar.width("handle"),
            if any_pointer_is_coarse() { 96.0 } else { 72.0 }
        );

        nav.class_list().add_1("controls-expanded").unwrap();
        assert_ne!(bar.computed("bar", "padding-left"), "0px");
        if !any_pointer_is_coarse() {
            assert_eq!(bar.computed("handle", "display"), "none");
        }
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn hover_opens_after_the_intent_delay_and_closes_after_the_leave_delay() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();

        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS - 100).await;
        assert!(!is_open(&nav), "still inside the intent delay");
        TimeoutFuture::new(100 + SETTLE_MS).await;
        assert!(is_open(&nav));

        hover.leave("mouse");
        TimeoutFuture::new(HOVER_CLOSE_DELAY_MS - 100).await;
        assert!(is_open(&nav), "still inside the leave delay");
        hover.enter("mouse");
        TimeoutFuture::new(100 + SETTLE_MS).await;
        assert!(is_open(&nav), "coming back cancels the pending close");

        hover.leave("mouse");
        TimeoutFuture::new(HOVER_CLOSE_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav));
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn touch_and_pen_never_open_the_bar_on_hover() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();
        hover.enter("touch");
        hover.enter("pen");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav));
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn leaving_before_the_intent_delay_never_opens_the_bar() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();
        hover.enter("mouse");
        hover.leave("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav));
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn a_press_in_the_bar_cancels_the_pending_open_until_the_next_enter() {
        let nav = mount_nav();
        let (_vdom, touch_latch, customize) = probe();
        let hover = Rc::new(BarHoverIntent::default());
        let shrink = install_action_bar_shrink(hover.clone(), touch_latch, customize, || false);

        hover.enter("mouse");
        pointer_down_on(&nav.first_element_child().unwrap());
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav), "a press keeps kept buttons where they are");

        hover.leave("mouse");
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav), "the next real enter opens again");
        drop(shrink);
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn a_repeat_enter_with_no_leave_does_not_rearm_a_cancelled_open() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();

        hover.enter("mouse");
        hover.cancel_pending_open();
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav), "the repeat enter is the DOM changing");
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn a_move_outside_the_bar_stands_in_for_a_missing_leave() {
        let nav = mount_nav();
        let hover = Rc::new(BarHoverIntent::default());
        let on_move = hover.clone();
        let moves = WindowEventListener::with_event("mousemove", false, move |e| {
            on_move.note_pointer_move(&e)
        });

        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav));

        event_on("mousemove", &nav.first_element_child().unwrap());
        TimeoutFuture::new(HOVER_CLOSE_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav), "a move inside the bar is not a leave");

        event_on("mousemove", &gloo_utils::document().body().unwrap());
        TimeoutFuture::new(HOVER_CLOSE_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav), "no leave arrived, and the bar still closed");
        drop(moves);
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn stale_inside_state_recovers_on_the_next_move_outside_the_bar() {
        let nav = mount_nav();
        let hover = Rc::new(BarHoverIntent::default());
        let on_move = hover.clone();
        let moves = WindowEventListener::with_event("mousemove", false, move |e| {
            on_move.note_pointer_move(&e)
        });

        hover.enter("mouse");
        hover.cancel_pending_open();
        event_on("mousemove", &gloo_utils::document().body().unwrap());
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav), "the enter after the recovery is a real one");
        drop(moves);
        nav.remove();
    }

    #[wasm_bindgen_test]
    async fn dismissing_holds_until_the_next_real_enter() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav));

        hover.dismiss();
        assert!(!is_open(&nav));
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav), "a repeat enter does not undo Escape");

        hover.leave("mouse");
        hover.enter("mouse");
        TimeoutFuture::new(HOVER_OPEN_DELAY_MS + SETTLE_MS).await;
        assert!(is_open(&nav), "a real leave and enter open again");
        nav.remove();
    }

    #[wasm_bindgen_test]
    fn a_press_outside_the_bar_clears_the_latch_and_a_press_inside_keeps_it() {
        let nav = mount_nav();
        let (_vdom, mut touch_latch, customize) = probe();
        let hover = Rc::new(BarHoverIntent::default());
        let shrink = install_action_bar_shrink(hover, touch_latch, customize, || false);
        touch_latch.set(true);

        pointer_down_on(&nav.first_element_child().unwrap());
        assert!(*touch_latch.peek());

        pointer_down_on(&gloo_utils::document().body().unwrap());
        assert!(!*touch_latch.peek());
        drop(shrink);
        nav.remove();
    }

    #[wasm_bindgen_test]
    fn escape_closes_a_hover_opened_bar_only_with_nothing_else_to_peel() {
        let nav = mount_nav();
        let (_vdom, touch_latch, mut customize) = probe();
        let popover = Rc::new(std::cell::Cell::new(true));
        let popover_read = popover.clone();
        let hover = Rc::new(BarHoverIntent::default());
        let shrink = install_action_bar_shrink(hover.clone(), touch_latch, customize, move || {
            popover_read.get()
        });
        set_hover_open(true, &hover.snapped_at);

        press_escape();
        assert!(is_open(&nav), "Escape peels the popover first");

        popover.set(false);
        customize.set(true);
        press_escape();
        assert!(is_open(&nav), "Escape leaves customize mode first");

        customize.set(false);
        press_escape();
        assert!(!is_open(&nav));
        drop(shrink);
        nav.remove();
    }

    #[wasm_bindgen_test]
    fn a_mouse_enter_opens_a_bare_handle_at_once() {
        let nav = mount_nav();
        nav.class_list().add_1("shrink-empty").unwrap();
        let hover = BarHoverIntent::default();
        hover.enter("touch");
        assert!(!is_open(&nav));
        hover.enter("mouse");
        assert!(is_open(&nav), "nothing under the pointer to protect");
        nav.remove();
    }

    fn click_reaches(target: &web_sys::Element, detail: u32) -> bool {
        let hits = Rc::new(Cell::new(0u32));
        let seen = hits.clone();
        let on_click = Closure::<dyn FnMut()>::new(move || seen.set(seen.get() + 1));
        target
            .add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())
            .unwrap();
        let event: web_sys::Event = js_sys::eval(&format!(
            "new MouseEvent('click',{{bubbles:true,cancelable:true,detail:{detail}}})"
        ))
        .unwrap()
        .unchecked_into();
        let not_prevented = target.dispatch_event(&event).unwrap();
        target
            .remove_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())
            .unwrap();
        assert_eq!(not_prevented, hits.get() == 1);
        hits.get() == 1
    }

    #[wasm_bindgen_test]
    async fn clicks_pressed_soon_after_a_snap_are_swallowed_and_no_others() {
        let nav = mount_nav();
        let button = nav.first_element_child().unwrap();
        let body: web_sys::Element = gloo_utils::document().body().unwrap().into();
        let (_vdom, touch_latch, customize) = probe();
        let hover = Rc::new(BarHoverIntent::default());
        let shrink = install_action_bar_shrink(hover.clone(), touch_latch, customize, || false);

        pointer_down_on(&button);
        assert!(click_reaches(&button, 1), "no snap yet");

        pointer_down_on(&button);
        TimeoutFuture::new(5).await;
        set_hover_open(true, &hover.snapped_at);
        assert!(click_reaches(&button, 1), "pressed before the snap");

        pointer_down_on(&button);
        assert!(!click_reaches(&button, 1), "pressed after the snap");
        pointer_down_on(&button);
        assert!(!click_reaches(&button, 1), "a second press in the window");
        assert!(click_reaches(&button, 0), "keyboard activation");
        assert!(click_reaches(&body, 1), "outside the bar");

        TimeoutFuture::new(HOVER_SNAP_CLICK_GUARD_MS as u32 + SETTLE_MS).await;
        assert!(
            !click_reaches(&button, 1),
            "its release came late, and the press still aimed at the old layout"
        );
        pointer_down_on(&button);
        assert!(click_reaches(&button, 1), "the window has passed");

        nav.class_list().add_1("controls-expanded").unwrap();
        set_hover_open(false, &hover.snapped_at);
        pointer_down_on(&button);
        assert!(
            click_reaches(&button, 1),
            "an attribute flip on an expanded bar moves nothing, as with shrinking off"
        );
        drop(shrink);
        nav.remove();
    }

    const EXPANDED: BarLayout = BarLayout { expanded: true };
    const COLLAPSED: BarLayout = BarLayout { expanded: false };

    fn slot_button_has_focus() -> bool {
        gloo_utils::document()
            .active_element()
            .is_some_and(|el| el.id() == "slot-button")
    }

    fn focus_slot_button_with_no_ring() {
        js_sys::eval("document.getElementById('slot-button').focus({focusVisible:false})").unwrap();
    }

    // A browser that ignores the option cannot stage a mouse's focus.
    fn slot_button_takes_focus_with_no_ring(bar: &BarFixture) -> bool {
        focus_slot_button_with_no_ring();
        !bar.el("slot-button").matches(":focus-visible").unwrap()
    }

    #[wasm_bindgen_test]
    fn collapsing_blurs_a_clipped_control_that_holds_focus_with_no_ring() {
        let bar = BarFixture::mount();
        let hover = BarHoverIntent::default();
        set_hover_open(true, &hover.snapped_at);
        if slot_button_takes_focus_with_no_ring(&bar) {
            set_hover_open(false, &hover.snapped_at);
            assert!(!slot_button_has_focus());
        }
        bar.remove();
    }

    #[wasm_bindgen_test]
    fn the_blur_leaves_a_control_on_an_open_bar_alone() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        for class in ["controls-expanded", "customize-mode"] {
            nav.class_list().add_1(class).unwrap();
            if slot_button_takes_focus_with_no_ring(&bar) {
                blur_clipped_focus();
                assert!(slot_button_has_focus(), "{class}");
            }
            bar.el("slot-button").blur().unwrap();
            nav.class_list().remove_1(class).unwrap();
        }
        nav.set_attribute(HOVER_OPEN_ATTR, "").unwrap();
        if slot_button_takes_focus_with_no_ring(&bar) {
            blur_clipped_focus();
            assert!(slot_button_has_focus(), "{HOVER_OPEN_ATTR}");
        }
        bar.el("slot-button").blur().unwrap();
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn a_class_driven_collapse_stamps_and_blurs() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        let hover = BarHoverIntent::default();
        nav.class_list().add_1("controls-expanded").unwrap();
        hover.note_layout(EXPANDED);

        if slot_button_takes_focus_with_no_ring(&bar) {
            nav.class_list().remove_1("controls-expanded").unwrap();
            hover.note_layout(COLLAPSED);
            assert!(hover.snapped_at.get().is_some());
            TimeoutFuture::new(SETTLE_MS).await;
            assert!(!slot_button_has_focus());
        }
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn a_restore_queued_before_the_collapse_still_ends_blurred() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        let hover = BarHoverIntent::default();
        nav.class_list().add_1("controls-expanded").unwrap();
        hover.note_layout(EXPANDED);
        nav.class_list().remove_1("controls-expanded").unwrap();

        let landed = Rc::new(Cell::new(false));
        let seen = landed.clone();
        let restore = Timeout::new(0, move || {
            focus_slot_button_with_no_ring();
            seen.set(slot_button_has_focus());
        });
        hover.note_layout(COLLAPSED);
        TimeoutFuture::new(SETTLE_MS).await;
        drop(restore);

        assert!(landed.get(), "premise: the restore reached the control");
        assert!(!slot_button_has_focus());
        bar.remove();
    }

    #[wasm_bindgen_test]
    fn the_bar_opening_records_nothing() {
        let bar = BarFixture::mount();
        let hover = BarHoverIntent::default();
        hover.note_layout(COLLAPSED);
        assert_eq!(hover.snapped_at.get(), None, "the first layout is no move");
        hover.note_layout(EXPANDED);
        assert_eq!(hover.snapped_at.get(), None);
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn a_bar_still_open_under_the_falling_class_records_nothing() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        let hover = BarHoverIntent::default();
        nav.class_list().add_1("controls-expanded").unwrap();
        nav.set_attribute(HOVER_OPEN_ATTR, "").unwrap();
        hover.note_layout(EXPANDED);

        if slot_button_takes_focus_with_no_ring(&bar) {
            nav.class_list().remove_1("controls-expanded").unwrap();
            hover.note_layout(COLLAPSED);
            TimeoutFuture::new(SETTLE_MS).await;
            assert_eq!(hover.snapped_at.get(), None, "nothing moved");
            assert!(slot_button_has_focus());
        }
        bar.el("slot-button").blur().unwrap();
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn a_class_driven_collapse_leaves_keyboard_focus_alone() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        let button = bar.el("slot-button");
        let hover = BarHoverIntent::default();
        nav.class_list().add_1("controls-expanded").unwrap();
        hover.note_layout(EXPANDED);
        button.focus().unwrap();
        assert!(button.matches(":focus-visible").unwrap());

        nav.class_list().remove_1("controls-expanded").unwrap();
        hover.note_layout(COLLAPSED);
        TimeoutFuture::new(SETTLE_MS).await;
        assert!(slot_button_has_focus());
        assert_eq!(hover.snapped_at.get(), None, "focus holds the bar open");
        button.blur().unwrap();
        bar.remove();
    }

    #[wasm_bindgen_test]
    async fn a_mouse_click_on_the_handle_opens_at_once_and_mouse_off_shrinks() {
        let nav = mount_nav();
        let hover = BarHoverIntent::default();
        hover.enter("mouse");
        hover.cancel_pending_open();
        assert!(!is_open(&nav));

        hover.open_now();
        assert!(is_open(&nav));
        assert!(hover.snapped_at.get().is_some());

        hover.leave("mouse");
        TimeoutFuture::new(HOVER_CLOSE_DELAY_MS + SETTLE_MS).await;
        assert!(!is_open(&nav));
        nav.remove();
    }

    #[wasm_bindgen_test]
    fn collapsing_leaves_keyboard_focus_on_a_clipped_control() {
        let bar = BarFixture::mount();
        let nav = bar.el("bar");
        let button = bar.el("slot-button");
        let hover = BarHoverIntent::default();
        set_hover_open(true, &hover.snapped_at);
        button.focus().unwrap();
        assert!(button.matches(":focus-visible").unwrap());

        set_hover_open(false, &hover.snapped_at);
        blur_clipped_focus();
        assert!(!nav.has_attribute(HOVER_OPEN_ATTR));
        assert_eq!(
            gloo_utils::document().active_element().map(|el| el.id()),
            Some("slot-button".to_string())
        );
        button.blur().unwrap();
        bar.remove();
    }
}
