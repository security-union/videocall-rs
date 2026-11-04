// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 3173: start, replace and stop the local share from the action bar.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::components::attendants::{
    show_share_toast, try_set, PreAcquiredScreenStream, ScreenShareState, ScreenShareToastState,
};
use crate::components::video_control_buttons::{ShareButtonState, SCREEN_SHARE_TRIGGER_ID};

/// The capture picked with "Share something else", kept apart from a new
/// share's [`PreAcquiredScreenStream`].
#[derive(Clone, Default)]
pub struct ScreenReplacementCell(Rc<RefCell<Option<web_sys::MediaStream>>>);

/// Hold `stream` in `cell`, stopping any capture it displaces.
fn hold(cell: &RefCell<Option<web_sys::MediaStream>>, stream: web_sys::MediaStream) {
    if let Some(old) = cell.borrow_mut().replace(stream) {
        stop_stream_tracks(&old);
    }
}

impl ScreenReplacementCell {
    pub fn put(&self, stream: web_sys::MediaStream) {
        hold(&self.0, stream);
    }

    pub fn take(&self) -> Option<web_sys::MediaStream> {
        self.0.borrow_mut().take()
    }

    /// Stop a capture no share will use.
    pub fn clear(&self) {
        if let Some(old) = self.take() {
            stop_stream_tracks(&old);
        }
    }
}

pub fn share_button_state(state: &ScreenShareState, replacing: bool) -> ShareButtonState {
    match state {
        ScreenShareState::Idle => ShareButtonState::Idle,
        ScreenShareState::Requesting | ScreenShareState::StreamReady => {
            ShareButtonState::Requesting
        }
        ScreenShareState::Active if replacing => ShareButtonState::Replacing,
        ScreenShareState::Active => ShareButtonState::Active,
    }
}

/// Whether the share control keeps the action bar shown: while its menu is
/// open, and while the browser picker for a replacement covers the page.
pub fn share_control_pins_bar(menu_open: bool, replacing: bool) -> bool {
    menu_open || replacing
}

#[derive(Clone, Copy)]
pub struct ShareActions {
    pub menu_open: Signal<bool>,
    pub replacing: Signal<bool>,
    /// Bumped once a replacement capture is in the [`ScreenReplacementCell`].
    pub replace_nonce: Signal<u32>,
    /// Bumped when a share starts or ends; a pick made for an earlier share is
    /// stopped.
    pub generation: Signal<u32>,
    pub start: EventHandler<()>,
    pub replace: EventHandler<()>,
    pub stop: EventHandler<()>,
}

pub fn use_share_actions(
    start_cell: PreAcquiredScreenStream,
    replacement: ScreenReplacementCell,
    mut state: Signal<ScreenShareState>,
    meeting_live: Memo<bool>,
    toast: Signal<Option<ScreenShareToastState>>,
    toast_timer: Signal<Option<Timeout>>,
    announce: EventHandler<String>,
) -> ShareActions {
    let menu_open = use_signal(|| false);
    let mut replacing = use_signal(|| false);
    let replace_nonce = use_signal(|| 0u32);
    let generation = use_signal(|| 0u32);
    let mut start_ticket = use_signal(|| 0u32);
    use_drop({
        let start_cell = start_cell.clone();
        let replacement = replacement.clone();
        move || {
            replacement.clear();
            if let Some(stream) = start_cell.borrow_mut().take() {
                stop_stream_tracks(&stream);
            }
        }
    });
    let start = use_callback({
        let start_cell = start_cell.clone();
        move |_: ()| {
            if *state.peek() != ScreenShareState::Idle {
                return;
            }
            let Some(pick) = Pick::request(None) else {
                return;
            };
            state.set(ScreenShareState::Requesting);
            let ticket = start_ticket.peek().wrapping_add(1);
            start_ticket.set(ticket);
            let start_cell = start_cell.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let requesting = || {
                    start_ticket.try_peek().is_ok_and(|t| *t == ticket)
                        && state
                            .try_peek()
                            .is_ok_and(|s| *s == ScreenShareState::Requesting)
                };
                match pick.resolve().await {
                    Ok(stream) => {
                        let requesting = requesting();
                        if requesting && meeting_live.try_peek().is_ok_and(|live| *live) {
                            hold(&start_cell, stream);
                            try_set(state, ScreenShareState::StreamReady);
                        } else {
                            stop_stream_tracks(&stream);
                            if requesting {
                                try_set(state, ScreenShareState::Idle);
                            }
                        }
                    }
                    Err(e) => {
                        log_picker_error(&e);
                        if requesting() {
                            try_set(state, ScreenShareState::Idle);
                        }
                    }
                }
            });
        }
    });
    let replace = use_callback(move |_: ()| {
        if *replacing.peek() || *state.peek() != ScreenShareState::Active {
            return;
        }
        let Some(pick) = Pick::request(Some(FocusBehavior::NoFocusChange)) else {
            show_share_toast(toast, toast_timer, ScreenShareToastState::ReplaceFailed);
            return;
        };
        replacing.set(true);
        let share = *generation.peek();
        let replacement = replacement.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let picked = pick.resolve().await;
            try_set(replacing, false);
            let same_share = generation.try_peek().is_ok_and(|g| *g == share)
                && state
                    .try_peek()
                    .is_ok_and(|s| *s == ScreenShareState::Active)
                && meeting_live.try_peek().is_ok_and(|live| *live);
            match picked {
                Ok(stream) if same_share => {
                    replacement.put(stream);
                    if let Ok(next) = replace_nonce.try_peek().map(|n| n.wrapping_add(1)) {
                        try_set(replace_nonce, next);
                    }
                }
                Ok(stream) => {
                    stop_stream_tracks(&stream);
                    show_share_toast(toast, toast_timer, ScreenShareToastState::ReplaceAbandoned);
                }
                Err(e) if is_picker_cancel(&e) => {}
                Err(e) => {
                    log::error!("getDisplayMedia for a replacement rejected: {e:?}");
                    let toast_state = if same_share {
                        ScreenShareToastState::ReplaceFailed
                    } else {
                        ScreenShareToastState::ReplaceAbandoned
                    };
                    show_share_toast(toast, toast_timer, toast_state);
                }
            }
        });
    });
    let stop = use_callback(move |_: ()| {
        if *state.peek() == ScreenShareState::Active {
            state.set(ScreenShareState::Idle);
            announce.call("You stopped sharing".to_string());
        }
    });
    ShareActions {
        menu_open,
        replacing,
        replace_nonce,
        generation,
        start,
        replace,
        stop,
    }
}

/// `CaptureController.setFocusBehavior` values.
#[derive(Clone, Copy)]
enum FocusBehavior {
    NoFocusChange,
}

struct Pick {
    promise: js_sys::Promise,
    devices: web_sys::MediaDevices,
    focus: Option<FocusBehavior>,
    controller: Option<JsValue>,
}

impl Pick {
    fn request(focus: Option<FocusBehavior>) -> Option<Self> {
        let devices = match gloo_utils::window().navigator().media_devices() {
            Ok(devices) => devices,
            Err(e) => {
                log::error!("Failed to get media devices: {e:?}");
                return None;
            }
        };
        // issue 1973: the capture resolution ceiling (max 1920x1080).
        match display_media(&devices, true, focus) {
            Ok((promise, controller)) => Some(Self {
                promise,
                devices,
                focus,
                controller,
            }),
            Err(e) => {
                log::error!("getDisplayMedia failed synchronously: {e:?}");
                None
            }
        }
    }

    /// On OverconstrainedError the pick is retried once without the ceiling
    /// (issue 1973).
    async fn resolve(self) -> Result<web_sys::MediaStream, JsValue> {
        let mut controller = self.controller;
        let picked = match JsFuture::from(self.promise).await {
            Err(e)
                if videocall_client::should_retry_screen_capture_without_ceiling(
                    &js_error_name(&e),
                    false,
                ) =>
            {
                log::warn!("issue 1973: getDisplayMedia rejected the capture resolution ceiling (OverconstrainedError); retrying without max");
                match display_media(&self.devices, false, self.focus) {
                    Ok((promise, retry_controller)) => {
                        controller = retry_controller;
                        JsFuture::from(promise).await
                    }
                    Err(e2) => Err(e2),
                }
            }
            picked => picked,
        };
        if let (Ok(_), Some(controller), Some(focus)) = (&picked, &controller, self.focus) {
            set_focus_behavior(controller, focus);
        }
        picked.map(JsCast::unchecked_into)
    }
}

fn display_media(
    devices: &web_sys::MediaDevices,
    ceiling: bool,
    focus: Option<FocusBehavior>,
) -> Result<(js_sys::Promise, Option<JsValue>), JsValue> {
    let constraints = videocall_client::screen_capture_display_constraints(ceiling);
    let controller = focus.and_then(|_| capture_controller());
    if let Some(controller) = &controller {
        js_sys::Reflect::set(&constraints, &JsValue::from_str("controller"), controller)?;
    }
    let promise = devices.get_display_media_with_constraints(&constraints)?;
    Ok((promise, controller))
}

/// A new `CaptureController`, where the browser has one (Chromium).
fn capture_controller() -> Option<JsValue> {
    let class = js_sys::Reflect::get(&gloo_utils::window(), &"CaptureController".into()).ok()?;
    let class = class.dyn_into::<js_sys::Function>().ok()?;
    js_sys::Reflect::construct(&class, &js_sys::Array::new()).ok()
}

fn set_focus_behavior(controller: &JsValue, focus: FocusBehavior) {
    let value = match focus {
        FocusBehavior::NoFocusChange => "no-focus-change",
    };
    let set = js_sys::Reflect::get(controller, &"setFocusBehavior".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    if let Some(Err(e)) = set.map(|f| f.call1(controller, &value.into())) {
        log::warn!("setFocusBehavior({value}) refused: {e:?}");
    }
}

fn js_error_name(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}

fn is_picker_cancel(e: &JsValue) -> bool {
    js_error_name(e) == "NotAllowedError"
}

fn log_picker_error(e: &JsValue) {
    if is_picker_cancel(e) {
        log::info!("User cancelled screen sharing");
    } else {
        log::error!("getDisplayMedia rejected: {e:?}");
    }
}

fn stop_stream_tracks(stream: &web_sys::MediaStream) {
    for track in stream.get_tracks().iter() {
        track.unchecked_into::<web_sys::MediaStreamTrack>().stop();
    }
}

/// The share ended: its replacement is dropped, a pick still in the picker
/// no longer counts, its menu closes, and focus inside the menu returns to the
/// button.
pub fn share_ended(actions: ShareActions, replacement: &ScreenReplacementCell) {
    replacement.clear();
    if let Ok(next) = actions.generation.try_peek().map(|g| g.wrapping_add(1)) {
        try_set(actions.generation, next);
    }
    refocus_from_vanishing_share_item(".overflow-share-group");
    if !actions.menu_open.try_peek().is_ok_and(|o| *o) {
        return;
    }
    let refocus = focus_in_share_menu();
    try_set(actions.menu_open, false);
    if refocus {
        focus_share_control();
    }
}

/// A share announced Started and is now Active: a new share, unlike an
/// encoder restart of a live one, does not inherit an earlier share's pending
/// pick.
pub fn share_started(actions: ShareActions, state: Signal<ScreenShareState>) {
    if state
        .try_peek()
        .is_ok_and(|s| *s != ScreenShareState::Active)
    {
        if let Ok(next) = actions.generation.try_peek().map(|g| g.wrapping_add(1)) {
            try_set(actions.generation, next);
        }
        refocus_from_vanishing_share_item("[data-testid='overflow-share-start']");
    }
    try_set(state, ScreenShareState::Active);
}

/// While the share cannot be published (Host is gone and sends no Stopped),
/// the share ends: every pick it has pending or still in the picker is
/// stopped, and a new share waiting for Host is dropped.
pub fn use_release_share_unless(
    publishing: bool,
    actions: ShareActions,
    start_cell: PreAcquiredScreenStream,
    replacement: ScreenReplacementCell,
    state: Signal<ScreenShareState>,
    own_stream: Signal<Option<web_sys::MediaStream>>,
) {
    use_effect(use_reactive!(|publishing| {
        if publishing {
            return;
        }
        share_ended(actions, &replacement);
        if let Some(stream) = start_cell.borrow_mut().take() {
            stop_stream_tracks(&stream);
        }
        if state.try_peek().is_ok_and(|s| *s != ScreenShareState::Idle) {
            try_set(state, ScreenShareState::Idle);
            try_set(own_stream, None);
        }
    }));
}

fn refocus_from_vanishing_share_item(item: &str) {
    if focus_within(&format!(".recent-actions-popover {item}")) {
        crate::components::recent_actions::focus_recents_return();
    } else if focus_within(&format!(".action-bar-overflow-popover {item}")) {
        crate::components::canvas_generator::focus_element_by_id("overflow-menu-trigger");
    }
}

pub fn focus_in_share_menu() -> bool {
    focus_within(".screen-share-menu")
}

fn focus_within(selector: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .and_then(|a| a.closest(selector).ok().flatten())
        .is_some()
}

/// Focus the share button, or the overflow trigger while the button is
/// hidden in the overflow.
pub fn focus_share_control() {
    use crate::components::canvas_generator::focus_element_by_id;
    focus_element_by_id(SCREEN_SHARE_TRIGGER_ID);
    let on_trigger = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .is_some_and(|a| a.id() == SCREEN_SHARE_TRIGGER_ID);
    if !on_trigger {
        focus_element_by_id("overflow-menu-trigger");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_share_button_offers_its_menu_only_while_a_share_is_live() {
        use ScreenShareState as S;
        use ShareButtonState as B;
        assert_eq!(share_button_state(&S::Idle, false), B::Idle);
        assert_eq!(share_button_state(&S::Requesting, false), B::Requesting);
        assert_eq!(share_button_state(&S::StreamReady, false), B::Requesting);
        assert_eq!(share_button_state(&S::Active, false), B::Active);
        assert_eq!(share_button_state(&S::Active, true), B::Replacing);
        assert_eq!(share_button_state(&S::Idle, true), B::Idle);
    }

    #[test]
    fn the_bar_stays_shown_while_the_menu_is_open_or_a_pick_covers_the_page() {
        assert!(!share_control_pins_bar(false, false));
        assert!(share_control_pins_bar(true, false));
        assert!(share_control_pins_bar(false, true));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    thread_local! {
        static INITIAL: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
        static CELLS: RefCell<Option<(PreAcquiredScreenStream, ScreenReplacementCell)>> =
            const { RefCell::new(None) };
    }

    fn js(src: &str) -> JsValue {
        js_sys::Function::new_no_args(src)
            .call0(&JsValue::NULL)
            .unwrap()
    }

    /// The picker answers with a fresh canvas capture (kept in
    /// `window.__pick`), rejects with `fail` (an OverconstrainedError only
    /// once), or with `fail == "hold"` queues each call for
    /// [`answer_picker`] / [`refuse_picker`], oldest first.
    fn stub_picker(fail: &str) {
        js(&format!(
            "window.__picks = []; window.__focus = null; window.__pick = null; window.__held = []; \
             window.CaptureController = class {{ setFocusBehavior(v) {{ window.__focus = v; }} }}; \
             const capture = () => {{ const canvas = document.createElement('canvas'); \
               canvas.getContext('2d'); window.__pick = canvas.captureStream(); \
               return window.__pick; }}; \
             navigator.mediaDevices.getDisplayMedia = (c) => {{ \
               window.__picks.push({{ max: c.video && c.video.width && c.video.width.max, \
                                      controller: !!c.controller }}); \
               if ('{fail}' === 'hold') \
                 return new Promise((ok, no) => {{ window.__held.push({{ ok, no }}); }}); \
               if ('{fail}' && (window.__picks.length === 1 || '{fail}' !== 'OverconstrainedError')) \
                 return Promise.reject(new DOMException('no', '{fail}')); \
               return Promise.resolve(capture()); }};"
        ));
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let mut mounted = use_signal(|| true);
        rsx! {
            button { id: "leave", onclick: move |_| mounted.set(false) }
            if mounted() {
                Meeting {}
            }
        }
    }

    #[allow(non_snake_case)]
    fn Meeting() -> Element {
        let mut state = use_signal(|| {
            if INITIAL.with(|i| i.get()) {
                ScreenShareState::Active
            } else {
                ScreenShareState::Idle
            }
        });
        let mut live = use_signal(|| true);
        let meeting_live = use_memo(move || live());
        let toast = use_signal(|| None);
        let timer = use_signal(|| None);
        let mut said = use_signal(String::new);
        let announce = use_callback(move |text: String| said.set(text));
        let start_cell: PreAcquiredScreenStream = use_hook(|| Rc::new(RefCell::new(None)));
        let replacement = use_hook(ScreenReplacementCell::default);
        let mut publishing = use_signal(|| true);
        let own_stream = use_signal(|| None::<web_sys::MediaStream>);
        let actions = use_share_actions(
            start_cell.clone(),
            replacement.clone(),
            state,
            meeting_live,
            toast,
            timer,
            announce,
        );
        use_release_share_unless(
            publishing(),
            actions,
            start_cell.clone(),
            replacement.clone(),
            state,
            own_stream,
        );
        let mut own_stream = own_stream;
        let replacing = actions.replacing;
        CELLS.with(|c| *c.borrow_mut() = Some((start_cell, replacement.clone())));
        let replaced = actions.replace_nonce;
        let mut menu_open = actions.menu_open;
        let toast_text = format!("{:?}", toast());
        rsx! {
            button { id: "start", onclick: move |_| actions.start.call(()) }
            button { id: "replace", onclick: move |_| actions.replace.call(()) }
            button { id: "stop", onclick: move |_| actions.stop.call(()) }
            button { id: "end", onclick: move |_| state.set(ScreenShareState::Idle) }
            button { id: "share-again", onclick: move |_| state.set(ScreenShareState::Active) }
            button {
                id: "end-meeting",
                onclick: move |_| {
                    live.set(false);
                    state.set(ScreenShareState::Idle);
                },
            }
            button { id: "lose-meeting", onclick: move |_| live.set(false) }
            button { id: "lose-stream", onclick: move |_| publishing.set(false) }
            button { id: "regain-stream", onclick: move |_| publishing.set(true) }
            button {
                id: "own-capture",
                onclick: move |_| {
                    let canvas: web_sys::HtmlCanvasElement = gloo_utils::document()
                        .create_element("canvas")
                        .unwrap()
                        .unchecked_into();
                    let _ = canvas.get_context("2d");
                    own_stream.set(canvas.capture_stream().ok());
                },
            }
            button {
                id: "new-share",
                onclick: move |_| {
                    state.set(ScreenShareState::StreamReady);
                    share_started(actions, state);
                },
            }
            button {
                id: "restart",
                onclick: move |_| share_started(actions, state),
            }
            button { id: "ended", onclick: move |_| share_ended(actions, &replacement) }
            button { id: "open-menu", onclick: move |_| menu_open.set(true) }
            button { id: SCREEN_SHARE_TRIGGER_ID }
            button { id: "outside" }
            if menu_open() {
                div { class: "screen-share-menu", button { id: "menu-item" } }
            }
            div { class: "action-bar-overflow-popover",
                crate::components::video_control_buttons::OverflowShareItems {
                    state: share_button_state(&state(), replacing()),
                    icon: rsx! {},
                    on_done: |_| {},
                    on_start: |_| {},
                    on_replace: |_| {},
                    on_stop: |_| {},
                }
            }
            button { id: "overflow-menu-trigger" }
            span { id: "state", "{state:?}" }
            span { id: "nonce", "{replaced}" }
            span { id: "toast", "{toast_text}" }
            span { id: "said", "{said}" }
            span { id: "own", "{own_stream.read().is_some()}" }
            span { id: "replacing", "{replacing}" }
        }
    }

    async fn mount_with(active: bool) -> web_sys::Element {
        INITIAL.with(|i| i.set(active));
        let root = gloo_utils::document().create_element("div").unwrap();
        gloo_utils::document()
            .body()
            .unwrap()
            .append_child(&root)
            .unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        for _ in 0..200 {
            if root.query_selector("#replace").unwrap().is_some() {
                break;
            }
            TimeoutFuture::new(10).await;
        }
        root
    }

    async fn mount() -> web_sys::Element {
        mount_with(true).await
    }

    fn el(root: &web_sys::Element, id: &str) -> web_sys::HtmlElement {
        root.query_selector(&format!("#{id}"))
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
    }

    fn focus(root: &web_sys::Element, selector: &str) {
        root.query_selector(selector)
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .focus()
            .unwrap();
    }

    fn press(root: &web_sys::Element, id: &str) {
        el(root, id).click();
    }

    async fn click(root: &web_sys::Element, id: &str) {
        press(root, id);
        TimeoutFuture::new(50).await;
    }

    async fn answer_picker() {
        js("const p = window.__held.shift(); window.__pick = null; \
            const c = document.createElement('canvas'); c.getContext('2d'); \
            window.__pick = c.captureStream(); p.ok(window.__pick);");
        TimeoutFuture::new(50).await;
    }

    async fn refuse_picker(name: &str) {
        js(&format!(
            "window.__held.shift().no(new DOMException('no', '{name}'));"
        ));
        TimeoutFuture::new(50).await;
    }

    fn text(root: &web_sys::Element, id: &str) -> String {
        el(root, id).text_content().unwrap_or_default()
    }

    fn focused() -> String {
        gloo_utils::document()
            .active_element()
            .map(|e| e.id())
            .unwrap_or_default()
    }

    fn replacement() -> Option<web_sys::MediaStream> {
        CELLS.with(|c| c.borrow().as_ref().and_then(|(_, r)| r.0.borrow().clone()))
    }

    fn started() -> Option<web_sys::MediaStream> {
        CELLS.with(|c| c.borrow().as_ref().and_then(|(s, _)| s.borrow().clone()))
    }

    fn ended(stream: &web_sys::MediaStream) -> bool {
        stream.get_tracks().iter().all(|t| {
            t.unchecked_into::<web_sys::MediaStreamTrack>()
                .ready_state()
                == web_sys::MediaStreamTrackState::Ended
        })
    }

    fn last_pick() -> web_sys::MediaStream {
        js("return window.__pick").unchecked_into()
    }

    #[wasm_bindgen_test]
    async fn a_replacement_is_picked_under_the_ceiling_without_moving_focus() {
        stub_picker("");
        let root = mount().await;
        click(&root, "replace").await;
        assert_eq!(text(&root, "nonce"), "1");
        let first = replacement().expect("the pick waits for Host");
        assert!(started().is_none(), "never as a new share's stream");
        let ceiling = videocall_client::screen_capture_display_constraints(true);
        let ceiling = js_sys::Reflect::get(&ceiling, &"video".into())
            .and_then(|v| js_sys::Reflect::get(&v, &"width".into()))
            .and_then(|w| js_sys::Reflect::get(&w, &"max".into()))
            .unwrap();
        assert!(ceiling.as_f64().is_some(), "premise: the ceiling has a max");
        let picks = js("return JSON.stringify(window.__picks)");
        assert_eq!(
            picks.as_string().unwrap(),
            format!(
                r#"[{{"max":{},"controller":true}}]"#,
                ceiling.as_f64().unwrap()
            )
        );
        assert_eq!(
            js("return window.__focus"),
            JsValue::from_str("no-focus-change")
        );

        click(&root, "replace").await;
        assert!(ended(&first), "a displaced pick is stopped");
        let second = replacement().unwrap();
        click(&root, "ended").await;
        assert!(replacement().is_none() && ended(&second));
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn an_overconstrained_pick_is_retried_once_without_the_ceiling() {
        stub_picker("OverconstrainedError");
        let root = mount().await;
        click(&root, "replace").await;
        let picks = js("return window.__picks.map((p) => p.max === undefined).join()");
        assert_eq!(picks.as_string().unwrap(), "false,true");
        assert!(replacement().is_some());
        assert_eq!(text(&root, "toast"), "None");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn leaving_the_meeting_stops_a_pick_no_share_took() {
        for picked_before_leaving in [true, false] {
            stub_picker("hold");
            let root = mount().await;
            press(&root, "replace");
            if picked_before_leaving {
                answer_picker().await;
                assert!(replacement().is_some(), "premise: the pick waits for Host");
            }
            click(&root, "leave").await;
            if !picked_before_leaving {
                answer_picker().await;
            }
            assert!(
                ended(&last_pick()),
                "picked before leaving: {picked_before_leaving}"
            );
            root.remove();
        }

        stub_picker("");
        let root = mount_with(false).await;
        click(&root, "start").await;
        let waiting = started().expect("premise: the start pick waits for Host");
        click(&root, "leave").await;
        assert!(ended(&waiting), "a start pick waiting for Host");
        assert!(started().is_none());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_pick_made_after_the_share_ended_is_stopped_and_said() {
        for (ending, state) in [("end", "Idle"), ("lose-meeting", "Active")] {
            stub_picker("hold");
            let root = mount().await;
            press(&root, "replace");
            click(&root, ending).await;
            answer_picker().await;
            assert!(ended(&last_pick()), "{ending}");
            assert_eq!(text(&root, "state"), state, "{ending}");
            assert!(started().is_none() && replacement().is_none(), "{ending}");
            assert_eq!(text(&root, "nonce"), "0", "{ending}");
            assert_eq!(text(&root, "toast"), "Some(ReplaceAbandoned)", "{ending}");
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn a_refusal_after_the_share_ended_never_claims_it_is_still_live() {
        stub_picker("hold");
        let root = mount().await;
        press(&root, "replace");
        click(&root, "end").await;
        refuse_picker("NotReadableError").await;
        assert_eq!(text(&root, "toast"), "Some(ReplaceAbandoned)");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_new_share_never_inherits_a_pick_but_a_restart_keeps_it() {
        for (between, nonce) in [("new-share", "0"), ("restart", "1")] {
            stub_picker("hold");
            let root = mount().await;
            press(&root, "replace");
            click(&root, between).await;
            answer_picker().await;
            assert_eq!(text(&root, "nonce"), nonce, "{between}");
            assert_eq!(ended(&last_pick()), nonce == "0", "{between}");
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn only_the_latest_start_request_keeps_its_pick() {
        stub_picker("hold");
        let root = mount_with(false).await;
        for _ in 0..2 {
            press(&root, "start");
            click(&root, "end").await;
        }
        press(&root, "start");
        TimeoutFuture::new(50).await;
        refuse_picker("NotReadableError").await;
        assert_eq!(text(&root, "state"), "Requesting", "a stale refusal");
        answer_picker().await;
        let superseded = last_pick();
        assert!(ended(&superseded), "the superseded pick is stopped");
        assert_eq!(text(&root, "state"), "Requesting");
        answer_picker().await;
        assert_eq!(text(&root, "state"), "StreamReady");
        assert!(started().is_some_and(|s| js_sys::Object::is(&s, &last_pick())));
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn losing_the_right_to_share_stops_every_pick_it_has_pending() {
        stub_picker("");
        let root = mount().await;
        click(&root, "replace").await;
        let pick = replacement().expect("premise: the pick waits for Host");
        click(&root, "lose-stream").await;
        assert!(ended(&pick), "a pick waiting for Host");
        assert!(replacement().is_none());
        root.remove();

        stub_picker("hold");
        let root = mount().await;
        press(&root, "replace");
        click(&root, "lose-stream").await;
        answer_picker().await;
        assert!(ended(&last_pick()), "a pick still in the picker");
        assert!(replacement().is_none());
        assert_eq!(text(&root, "nonce"), "0");
        root.remove();

        stub_picker("hold");
        let root = mount_with(false).await;
        press(&root, "start");
        click(&root, "lose-stream").await;
        assert_eq!(text(&root, "state"), "Idle");
        answer_picker().await;
        assert!(ended(&last_pick()), "a new share's pick");
        assert!(started().is_none());
        root.remove();

        stub_picker("");
        let root = mount_with(false).await;
        click(&root, "start").await;
        let waiting = started().expect("premise: the share waits for Host");
        click(&root, "lose-stream").await;
        assert_eq!(text(&root, "state"), "Idle");
        assert!(ended(&waiting), "a new share waiting for Host");
        assert!(started().is_none());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_live_share_ends_when_it_can_no_longer_be_published() {
        let root = mount().await;
        click(&root, "own-capture").await;
        assert_eq!(text(&root, "own"), "true", "premise");
        click(&root, "lose-stream").await;
        assert_eq!(text(&root, "state"), "Idle");
        assert_eq!(text(&root, "own"), "false");
        click(&root, "regain-stream").await;
        assert_eq!(text(&root, "state"), "Idle", "nothing restarts on regain");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_replacement_in_the_picker_keeps_the_next_share_busy() {
        stub_picker("hold");
        let root = mount().await;
        press(&root, "replace");
        click(&root, "ended").await;
        click(&root, "new-share").await;
        assert_eq!(text(&root, "state"), "Active");
        assert_eq!(text(&root, "replacing"), "true");
        answer_picker().await;
        assert_eq!(text(&root, "replacing"), "false");
        assert!(ended(&last_pick()));
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_pick_made_for_an_earlier_share_never_lands_on_the_next_one() {
        stub_picker("hold");
        let root = mount().await;
        press(&root, "replace");
        click(&root, "ended").await;
        click(&root, "share-again").await;
        answer_picker().await;
        assert!(ended(&last_pick()));
        assert!(replacement().is_none());
        assert_eq!(text(&root, "nonce"), "0");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_new_share_picked_as_the_meeting_ends_is_stopped() {
        for ending in ["end-meeting", "lose-meeting"] {
            stub_picker("hold");
            let root = mount_with(false).await;
            press(&root, "start");
            click(&root, ending).await;
            answer_picker().await;
            assert!(ended(&last_pick()), "{ending}");
            assert!(started().is_none(), "{ending}");
            assert_eq!(text(&root, "state"), "Idle", "{ending}");
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn a_new_share_waits_for_host_and_replaces_none_unstopped() {
        stub_picker("");
        let root = mount_with(false).await;
        click(&root, "start").await;
        assert_eq!(text(&root, "state"), "StreamReady");
        let first = started().expect("the pick waits for Host");
        click(&root, "end").await;
        click(&root, "start").await;
        assert!(ended(&first), "a displaced start pick is stopped");
        assert!(started().is_some());
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_refused_pick_keeps_the_share_and_says_so_unless_cancelled() {
        for (fail, toast) in [
            ("NotAllowedError", "None"),
            ("NotReadableError", "Some(ReplaceFailed)"),
        ] {
            stub_picker(fail);
            let root = mount().await;
            click(&root, "replace").await;
            assert_eq!(text(&root, "state"), "Active", "{fail}");
            assert_eq!(text(&root, "toast"), toast, "{fail}");
            assert_eq!(text(&root, "nonce"), "0", "{fail}");
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn stopping_the_share_says_so() {
        let root = mount().await;
        click(&root, "stop").await;
        assert_eq!(text(&root, "state"), "Idle");
        assert_eq!(text(&root, "said"), "You stopped sharing");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_share_ending_under_the_overflow_hands_focus_to_its_trigger() {
        let root = mount().await;
        focus(&root, "[data-testid='overflow-share-replace']");
        press(&root, "ended");
        click(&root, "end").await;
        assert_eq!(focused(), "overflow-menu-trigger");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_share_ending_under_the_recents_menu_follows_its_focus_chain() {
        let root = mount().await;
        let trigger = gloo_utils::document().create_element("button").unwrap();
        trigger.set_id(crate::components::recent_actions::RECENT_ACTIONS_TRIGGER_ID);
        root.append_child(&trigger).unwrap();
        root.query_selector(".action-bar-overflow-popover")
            .unwrap()
            .unwrap()
            .class_list()
            .add_1("recent-actions-popover")
            .unwrap();
        focus(&root, "[data-testid='overflow-share-replace']");
        press(&root, "ended");
        click(&root, "end").await;
        assert_eq!(
            focused(),
            crate::components::recent_actions::RECENT_ACTIONS_TRIGGER_ID
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_share_starting_under_the_overflow_hands_focus_to_its_trigger() {
        for (from, to) in [
            (
                "[data-testid='overflow-share-start']",
                "overflow-menu-trigger",
            ),
            ("#outside", "outside"),
        ] {
            stub_picker("hold");
            let root = mount_with(false).await;
            click(&root, "start").await;
            assert_eq!(text(&root, "state"), "Requesting");
            focus(&root, from);
            click(&root, "new-share").await;
            assert!(
                root.query_selector("[data-testid='overflow-share-start']")
                    .unwrap()
                    .is_none(),
                "premise: the start item gave way to the choices"
            );
            assert_eq!(focused(), to, "focus on {from}");
            root.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn a_share_ending_returns_focus_from_its_menu_only() {
        for (from, to) in [
            ("menu-item", SCREEN_SHARE_TRIGGER_ID),
            ("outside", "outside"),
        ] {
            let root = mount().await;
            click(&root, "open-menu").await;
            el(&root, from).focus().unwrap();
            click(&root, "ended").await;
            assert_eq!(focused(), to, "focus on {from}");
            assert!(root.query_selector(".screen-share-menu").unwrap().is_none());
            root.remove();
        }
    }
}
