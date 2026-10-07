// SPDX-License-Identifier: MIT OR Apache-2.0

//! The participant's notice that a host muted their microphone or turned off
//! their camera. It stays until the participant dismisses it.

use crate::components::attendants::action_bar_announce_text;
use dioxus::prelude::*;
use wasm_bindgen::JsCast;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostActionKind {
    Mic,
    Camera,
}

impl HostActionKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Mic => "Host muted your microphone",
            Self::Camera => "Host turned off your camera",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Mic => "Use the microphone button to unmute.",
            Self::Camera => "Use the camera button to turn it back on.",
        }
    }

    pub fn back_on_hint(self) -> &'static str {
        match self {
            Self::Mic => "Your microphone is on again.",
            Self::Camera => "Your camera is on again.",
        }
    }

    pub fn testid(self) -> &'static str {
        match self {
            Self::Mic => "host-mute-notice",
            Self::Camera => "host-video-off-notice",
        }
    }

    pub fn close_testid(self) -> &'static str {
        match self {
            Self::Mic => "host-mute-notice-close",
            Self::Camera => "host-video-off-notice-close",
        }
    }

    pub fn close_label(self) -> &'static str {
        match self {
            Self::Mic => "Dismiss microphone muted message",
            Self::Camera => "Dismiss camera turned off message",
        }
    }

    pub fn status_testid(self) -> &'static str {
        match self {
            Self::Mic => "host-mute-status",
            Self::Camera => "host-video-off-status",
        }
    }

    fn toggle_testid(self) -> &'static str {
        match self {
            Self::Mic => "mic-toggle-button",
            Self::Camera => "camera-toggle-button",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Mic => Self::Camera,
            Self::Camera => Self::Mic,
        }
    }
}

const KINDS: [HostActionKind; 2] = [HostActionKind::Mic, HostActionKind::Camera];

fn show_host_action_notice(mut notice: Signal<Option<u32>>) {
    if let Ok(mut slot) = notice.try_write() {
        *slot = Some(slot.map_or(1, |n| n.wrapping_add(1)));
    }
}

/// A host turned this device off. The notice is shown, or bumped so it is
/// announced again, only if the device was on or pending on; `pending` is only
/// read.
pub fn apply_host_media_off(
    mut device: Signal<bool>,
    pending: Signal<bool>,
    notice: Signal<Option<u32>>,
) {
    let was_on = *device.peek() || *pending.peek();
    device.set(false);
    if was_on {
        show_host_action_notice(notice);
    }
}

/// The notice one Escape press dismisses: the focused one, else the mic notice,
/// else the camera notice. None while a modal dialog or fullscreen owns Escape.
pub fn notice_for_escape(
    mic_shown: bool,
    camera_shown: bool,
    focused: Option<HostActionKind>,
    escape_owned_elsewhere: bool,
) -> Option<HostActionKind> {
    if escape_owned_elsewhere {
        None
    } else if focused.is_some() {
        focused
    } else if mic_shown {
        Some(HostActionKind::Mic)
    } else if camera_shown {
        Some(HostActionKind::Camera)
    } else {
        None
    }
}

pub fn focused_notice() -> Option<HostActionKind> {
    let active = gloo_utils::document().active_element()?;
    let notice = active.closest(".host-action-notice").ok().flatten()?;
    let testid = notice.get_attribute("data-testid")?;
    KINDS.into_iter().find(|kind| kind.testid() == testid)
}

pub fn escape_owned_elsewhere() -> bool {
    let doc = gloo_utils::document();
    doc.fullscreen_element().is_some()
        || doc
            .query_selector("[aria-modal='true']")
            .ok()
            .flatten()
            .is_some()
}

fn focusable(doc: &web_sys::Document, testid: &str) -> Option<web_sys::HtmlElement> {
    doc.query_selector(&format!("[data-testid='{testid}']"))
        .ok()
        .flatten()
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
}

fn hand_off_focus(kind: HostActionKind) {
    let doc = gloo_utils::document();
    let closing_has_focus = doc
        .active_element()
        .and_then(|el| el.get_attribute("data-testid"))
        .is_some_and(|id| id == kind.close_testid());
    if !closing_has_focus {
        return;
    }
    let next = focusable(&doc, kind.other().close_testid())
        .or_else(|| focusable(&doc, kind.toggle_testid()));
    if let Some(next) = next {
        let _ = next.focus();
    }
}

pub fn dismiss_host_action_notice(kind: HostActionKind, mut notice: Signal<Option<u32>>) {
    hand_off_focus(kind);
    if let Ok(mut slot) = notice.try_write() {
        *slot = None;
    }
}

#[component]
pub fn HostActionNotice(
    kind: HostActionKind,
    notice: Signal<Option<u32>>,
    device_on: bool,
) -> Element {
    let current = notice();
    let status_text = current
        .map(|n| action_bar_announce_text(&format!("{}. {}", kind.title(), kind.hint()), n))
        .unwrap_or_default();
    let (hint, class) = if device_on {
        (
            kind.back_on_hint(),
            "peer-toast host-action-notice host-action-notice--resolved",
        )
    } else {
        (kind.hint(), "peer-toast host-action-notice")
    };
    rsx! {
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": kind.status_testid(),
            "{status_text}"
        }
        if current.is_some() {
            div { class, "data-testid": kind.testid(),
                span { class: "toast-icon",
                    if kind == HostActionKind::Mic {
                        svg {
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            "aria-hidden": "true",
                            line { x1: "1", y1: "1", x2: "23", y2: "23" }
                            path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
                            path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                            line { x1: "12", y1: "19", x2: "12", y2: "23" }
                            line { x1: "8", y1: "23", x2: "16", y2: "23" }
                        }
                    } else {
                        svg {
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            "aria-hidden": "true",
                            path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                            line { x1: "1", y1: "1", x2: "23", y2: "23" }
                        }
                    }
                }
                span { class: "toast-text",
                    span { class: "toast-name", "{kind.title()}" }
                    br {}
                    span { class: "toast-action", "{hint}" }
                }
                button {
                    r#type: "button",
                    class: "toast-close-btn",
                    "data-testid": kind.close_testid(),
                    "aria-label": kind.close_label(),
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        dismiss_host_action_notice(kind, notice);
                    },
                    svg {
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2.5",
                        stroke_linecap: "round",
                        "aria-hidden": "true",
                        line { x1: "18", y1: "6", x2: "6", y2: "18" }
                        line { x1: "6", y1: "6", x2: "18", y2: "18" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_dismisses_the_focused_notice_then_mic_then_camera() {
        use HostActionKind::{Camera, Mic};
        assert_eq!(
            notice_for_escape(true, true, Some(Camera), false),
            Some(Camera)
        );
        assert_eq!(notice_for_escape(true, true, None, false), Some(Mic));
        assert_eq!(notice_for_escape(false, true, None, false), Some(Camera));
        assert_eq!(notice_for_escape(false, false, None, false), None);
    }

    #[test]
    fn escape_leaves_the_notices_alone_while_a_modal_or_fullscreen_owns_it() {
        use HostActionKind::Camera;
        assert_eq!(notice_for_escape(true, true, None, true), None);
        assert_eq!(notice_for_escape(true, true, Some(Camera), true), None);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    const STYLE_CSS: &str = include_str!("../../static/style.css");
    const GLOBAL_CSS: &str = include_str!("../../static/global.css");
    const HARNESS_CLASS: &str = "host-action-notice-harness";

    fn slug(kind: HostActionKind) -> &'static str {
        match kind {
            HostActionKind::Mic => "mic",
            HostActionKind::Camera => "camera",
        }
    }

    #[component]
    fn Controls(
        kind: HostActionKind,
        on: Signal<bool>,
        pending: Signal<bool>,
        notice: Signal<Option<u32>>,
    ) -> Element {
        let s = slug(kind);
        rsx! {
            button {
                "data-testid": kind.toggle_testid(),
                onclick: move |_| {
                    let mut on = on;
                    let next = !*on.peek();
                    on.set(next);
                },
                "{s}"
            }
            button { id: "host-off-{s}", onclick: move |_| apply_host_media_off(on, pending, notice) }
            button {
                id: "pending-{s}",
                onclick: move |_| {
                    let mut pending = pending;
                    pending.set(true);
                },
            }
            span {
                id: "{s}-state",
                "data-shown": "{notice().is_some()}",
                "data-on": "{on()}",
                "data-pending": "{pending()}",
            }
        }
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let mic_on = use_signal(|| true);
        let camera_on = use_signal(|| true);
        let mic_pending = use_signal(|| false);
        let camera_pending = use_signal(|| false);
        let mic = use_signal(|| None::<u32>);
        let camera = use_signal(|| None::<u32>);
        let mut clicks = use_signal(|| 0u32);
        rsx! {
            Controls { kind: HostActionKind::Mic, on: mic_on, pending: mic_pending, notice: mic }
            Controls {
                kind: HostActionKind::Camera,
                on: camera_on,
                pending: camera_pending,
                notice: camera,
            }
            div {
                id: "click-sink",
                "data-clicks": "{clicks}",
                onclick: move |_| clicks += 1,
                div { class: "peer-toasts",
                    HostActionNotice { kind: HostActionKind::Mic, notice: mic, device_on: mic_on() }
                    HostActionNotice { kind: HostActionKind::Camera, notice: camera, device_on: camera_on() }
                }
            }
        }
    }

    async fn mount() -> web_sys::Element {
        let doc = gloo_utils::document();
        let stale = doc
            .query_selector_all(&format!(".{HARNESS_CLASS}"))
            .unwrap();
        for i in 0..stale.length() {
            if let Some(node) = stale.item(i) {
                node.unchecked_into::<web_sys::Element>().remove();
            }
        }
        let root = doc.create_element("div").unwrap();
        root.set_class_name(HARNESS_CLASS);
        doc.body().unwrap().append_child(&root).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(root.clone()),
        );
        eventually("the harness to render", || {
            find(&root, "#host-off-mic").is_some()
        })
        .await;
        root
    }

    fn find(root: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
        root.query_selector(selector).unwrap()
    }

    fn by_testid(root: &web_sys::Element, testid: &str) -> Option<web_sys::Element> {
        find(root, &format!("[data-testid='{testid}']"))
    }

    fn click(el: &web_sys::Element) {
        el.clone().unchecked_into::<web_sys::HtmlElement>().click();
    }

    fn click_id(root: &web_sys::Element, id: &str) {
        click(&find(root, &format!("#{id}")).unwrap_or_else(|| panic!("no #{id}")));
    }

    fn host_off(root: &web_sys::Element, kind: HostActionKind) {
        click_id(root, &format!("host-off-{}", slug(kind)));
    }

    fn toggle(root: &web_sys::Element, kind: HostActionKind) {
        click(&by_testid(root, kind.toggle_testid()).unwrap());
    }

    fn state(root: &web_sys::Element, kind: HostActionKind, attr: &str) -> Option<String> {
        find(root, &format!("#{}-state", slug(kind))).and_then(|el| el.get_attribute(attr))
    }

    fn status_text(root: &web_sys::Element, kind: HostActionKind) -> String {
        by_testid(root, kind.status_testid())
            .and_then(|el| el.text_content())
            .unwrap_or_default()
    }

    fn hint_text(root: &web_sys::Element, kind: HostActionKind) -> Option<String> {
        find(
            root,
            &format!("[data-testid='{}'] .toast-action", kind.testid()),
        )
        .and_then(|el| el.text_content())
    }

    fn is_focused(el: &web_sys::Element) -> bool {
        gloo_utils::document()
            .active_element()
            .is_some_and(|active| active.is_same_node(Some(el)))
    }

    async fn eventually(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            TimeoutFuture::new(10).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn host_off_shown(root: &web_sys::Element, kind: HostActionKind) -> web_sys::Element {
        host_off(root, kind);
        eventually(kind.testid(), || by_testid(root, kind.testid()).is_some()).await;
        by_testid(root, kind.testid()).unwrap()
    }

    #[wasm_bindgen_test]
    async fn a_shown_notice_has_its_text_a_labelled_close_button_and_an_announcement() {
        let root = mount().await;
        for kind in KINDS {
            let region = by_testid(&root, kind.status_testid()).expect("mounted while hidden");
            assert_eq!(region.get_attribute("role").as_deref(), Some("status"));
            assert_eq!(region.get_attribute("aria-live").as_deref(), Some("polite"));
            assert_eq!(status_text(&root, kind), "");
            assert!(by_testid(&root, kind.testid()).is_none());
        }

        let mic = host_off_shown(&root, HostActionKind::Mic).await;
        assert!(
            by_testid(&root, HostActionKind::Camera.testid()).is_none(),
            "the mic notice does not show the camera notice"
        );
        let camera = host_off_shown(&root, HostActionKind::Camera).await;
        assert!(
            mic.is_connected(),
            "the camera notice stacks with the mic notice"
        );

        for (kind, notice) in [
            (HostActionKind::Mic, &mic),
            (HostActionKind::Camera, &camera),
        ] {
            let classes = notice.class_list();
            assert!(classes.contains("peer-toast") && classes.contains("host-action-notice"));
            assert!(
                !classes.contains("toast-left"),
                "not styled as a leave toast"
            );
            assert!(
                notice.get_attribute("role").is_none(),
                "announced by the region only"
            );
            let text = |sel: &str| {
                notice
                    .query_selector(sel)
                    .unwrap()
                    .and_then(|el| el.text_content())
            };
            assert_eq!(text(".toast-name").as_deref(), Some(kind.title()));
            assert_eq!(text(".toast-action").as_deref(), Some(kind.hint()));
            let close = by_testid(notice, kind.close_testid()).expect("a close button");
            assert_eq!(close.tag_name(), "BUTTON");
            assert_eq!(
                close.get_attribute("aria-label").as_deref(),
                Some(kind.close_label())
            );
            let announced = status_text(&root, kind);
            assert!(announced.starts_with(kind.title()), "{announced}");
            assert!(announced.contains(kind.hint()), "{announced}");
            let focused = gloo_utils::document().active_element();
            assert!(
                !focused.is_some_and(|el| notice.contains(Some(&el))),
                "appearing does not take focus"
            );
        }
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_host_media_off_turns_the_device_off_and_notifies_only_if_it_was_on() {
        let root = mount().await;
        let cam = HostActionKind::Camera;
        toggle(&root, cam);
        eventually("the camera to be off", || {
            state(&root, cam, "data-on").as_deref() == Some("false")
        })
        .await;
        host_off(&root, cam);

        host_off_shown(&root, HostActionKind::Mic).await;
        assert_eq!(
            state(&root, HostActionKind::Mic, "data-on").as_deref(),
            Some("false")
        );
        assert!(
            by_testid(&root, cam.testid()).is_none(),
            "a device that was already off gets no notice"
        );
        assert_eq!(status_text(&root, cam), "");

        click_id(&root, "pending-camera");
        host_off_shown(&root, cam).await;
        assert_eq!(
            state(&root, cam, "data-pending").as_deref(),
            Some("true"),
            "pending is left as it was"
        );
        assert_eq!(state(&root, cam, "data-on").as_deref(), Some("false"));
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_notice_outlives_the_old_six_second_timer() {
        let root = mount().await;
        let notice = host_off_shown(&root, HostActionKind::Mic).await;
        TimeoutFuture::new(6_300).await;
        assert!(
            notice.is_connected(),
            "the host-mute notice must stay until it is dismissed"
        );
        assert_eq!(
            state(&root, HostActionKind::Mic, "data-shown").as_deref(),
            Some("true")
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_close_button_hides_the_notice_and_clears_its_state() {
        let root = mount().await;
        for kind in KINDS {
            host_off_shown(&root, kind).await;
        }
        let cam = HostActionKind::Camera;
        click(&by_testid(&root, cam.close_testid()).unwrap());
        eventually("the camera notice to close", || {
            by_testid(&root, cam.testid()).is_none()
        })
        .await;
        assert_eq!(state(&root, cam, "data-shown").as_deref(), Some("false"));
        assert_eq!(status_text(&root, cam), "");
        assert!(
            by_testid(&root, HostActionKind::Mic.testid()).is_some(),
            "closing one notice leaves the other"
        );
        let clicks = || {
            find(&root, "#click-sink")
                .and_then(|el| el.get_attribute("data-clicks"))
                .unwrap_or_default()
        };
        assert_eq!(
            clicks(),
            "0",
            "the close click must not reach the meeting container"
        );
        click(
            &find(
                &root,
                &format!(
                    "[data-testid='{}'] .toast-name",
                    HostActionKind::Mic.testid()
                ),
            )
            .unwrap(),
        );
        eventually("a notice body click to reach the sink", || clicks() == "1").await;
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_hint_follows_the_device_without_announcing_again() {
        let root = mount().await;
        let kind = HostActionKind::Mic;
        let notice = host_off_shown(&root, kind).await;
        let resolved = || notice.class_list().contains("host-action-notice--resolved");
        assert_eq!(hint_text(&root, kind).as_deref(), Some(kind.hint()));
        assert!(!resolved());
        let announced = status_text(&root, kind);

        toggle(&root, kind);
        eventually("the back-on hint", || {
            hint_text(&root, kind).as_deref() == Some(kind.back_on_hint())
        })
        .await;
        assert!(resolved(), "a device back on marks the notice resolved");
        assert_eq!(
            status_text(&root, kind),
            announced,
            "turning back on is not announced"
        );

        toggle(&root, kind);
        eventually("the normal hint", || {
            hint_text(&root, kind).as_deref() == Some(kind.hint())
        })
        .await;
        assert!(!resolved());
        assert_eq!(status_text(&root, kind), announced);
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_repeat_announces_again_only_when_the_device_was_back_on() {
        let root = mount().await;
        let kind = HostActionKind::Mic;
        let notice = host_off_shown(&root, kind).await;
        let first = status_text(&root, kind);

        host_off(&root, kind);
        toggle(&root, kind);
        eventually("the back-on hint", || {
            hint_text(&root, kind).as_deref() == Some(kind.back_on_hint())
        })
        .await;
        assert_eq!(
            status_text(&root, kind),
            first,
            "a repeat while off is silent"
        );

        host_off(&root, kind);
        eventually("a second announcement", || {
            status_text(&root, kind) != first
        })
        .await;
        assert!(status_text(&root, kind).starts_with(kind.title()));
        assert_eq!(hint_text(&root, kind).as_deref(), Some(kind.hint()));
        let count = root
            .query_selector_all(&format!("[data-testid='{}']", kind.testid()))
            .unwrap()
            .length();
        assert_eq!(count, 1, "a repeat does not stack a second notice");
        assert!(notice.is_connected(), "the same notice stays in place");
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn a_focused_close_button_hands_focus_to_the_other_notice_then_the_toggle() {
        let root = mount().await;
        for kind in KINDS {
            host_off_shown(&root, kind).await;
        }
        let focus_and_close = |kind: HostActionKind| {
            let close = by_testid(&root, kind.close_testid()).unwrap();
            close
                .clone()
                .unchecked_into::<web_sys::HtmlElement>()
                .focus()
                .unwrap();
            click(&close);
        };

        focus_and_close(HostActionKind::Mic);
        eventually("the mic notice to close", || {
            by_testid(&root, HostActionKind::Mic.testid()).is_none()
        })
        .await;
        let camera_close = by_testid(&root, HostActionKind::Camera.close_testid()).unwrap();
        assert!(
            is_focused(&camera_close),
            "focus moves to the remaining notice"
        );

        focus_and_close(HostActionKind::Camera);
        eventually("the camera notice to close", || {
            by_testid(&root, HostActionKind::Camera.testid()).is_none()
        })
        .await;
        let toggle = by_testid(&root, HostActionKind::Camera.toggle_testid()).unwrap();
        assert!(
            is_focused(&toggle),
            "then to the matching toggle, not <body>"
        );
        root.remove();
    }

    #[wasm_bindgen_test]
    async fn the_escape_readers_see_the_focused_notice_and_a_modal_dialog() {
        let root = mount().await;
        for kind in KINDS {
            host_off_shown(&root, kind).await;
        }
        let camera_close = by_testid(&root, HostActionKind::Camera.close_testid()).unwrap();
        camera_close
            .unchecked_ref::<web_sys::HtmlElement>()
            .focus()
            .unwrap();
        assert_eq!(focused_notice(), Some(HostActionKind::Camera));
        camera_close
            .unchecked_ref::<web_sys::HtmlElement>()
            .blur()
            .unwrap();
        assert_eq!(focused_notice(), None);

        assert!(!escape_owned_elsewhere());
        let dialog = gloo_utils::document().create_element("div").unwrap();
        dialog.set_attribute("aria-modal", "true").unwrap();
        root.append_child(&dialog).unwrap();
        assert!(escape_owned_elsewhere());
        root.remove();
    }

    fn add_sheet(doc: &web_sys::Document, css: &str) -> web_sys::Element {
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(css));
        doc.head().unwrap().append_child(&style).unwrap();
        style
    }

    fn computed_in(window: &web_sys::Window, el: &web_sys::Element, prop: &str) -> String {
        window
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value(prop)
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn the_shipped_css_keeps_the_notice_clickable_unblurred_and_never_fading() {
        let doc = gloo_utils::document();
        let style = add_sheet(&doc, STYLE_CSS);
        let global = add_sheet(&doc, GLOBAL_CSS);
        let root = mount().await;
        let window = gloo_utils::window();
        let computed = |el: &web_sys::Element, prop: &str| computed_in(&window, el, prop);
        let container = find(&root, ".peer-toasts").unwrap();
        assert_eq!(
            computed(&container, "pointer-events"),
            "none",
            "style.css did not load, so this test guards nothing"
        );
        let token_as = |prop: &str, token: &str| {
            let probe = doc.create_element("span").unwrap();
            probe
                .set_attribute("style", &format!("{prop}: var({token})"))
                .unwrap();
            root.append_child(&probe).unwrap();
            let value = computed(&probe, prop);
            probe.remove();
            value
        };
        let token_color = |token: &str| token_as("color", token);
        for kind in KINDS {
            let notice = host_off_shown(&root, kind).await;
            let animation = computed(&notice, "animation-name");
            assert!(
                animation.contains("toast-enter") && !animation.contains("toast-exit"),
                "{kind:?}: animation-name is {animation:?}"
            );
            assert_eq!(computed(&notice, "pointer-events"), "auto", "{kind:?}");
            assert_eq!(computed(&notice, "backdrop-filter"), "none", "{kind:?}");
            let hint = notice.query_selector(".toast-action").unwrap().unwrap();
            assert_eq!(
                computed(&hint, "color"),
                token_color("--on-dark-text-secondary"),
                "{kind:?}: hint colour"
            );
            let icon = notice.query_selector(".toast-icon svg").unwrap().unwrap();
            assert_eq!(
                computed(&icon, "color"),
                token_color("--warning"),
                "{kind:?}: icon tint"
            );
        }

        let kind = HostActionKind::Mic;
        toggle(&root, kind);
        eventually("the resolved notice", || {
            by_testid(&root, kind.testid())
                .is_some_and(|n| n.class_list().contains("host-action-notice--resolved"))
        })
        .await;
        let icon = find(
            &root,
            &format!("[data-testid='{}'] .toast-icon", kind.testid()),
        )
        .unwrap();
        let svg = icon.query_selector("svg").unwrap().unwrap();
        assert_eq!(
            computed(&svg, "color"),
            token_color("--on-dark-text-muted"),
            "resolved icon tint"
        );
        assert_eq!(
            computed(&icon, "background-color"),
            token_as("background-color", "--toast-icon-neutral-bg"),
            "resolved icon background"
        );
        root.remove();
        global.remove();
        style.remove();
    }

    #[wasm_bindgen_test]
    async fn the_recording_bar_pushes_the_toast_stack_below_it_at_every_width() {
        let doc = gloo_utils::document();
        let frame = doc.create_element("iframe").unwrap();
        frame.set_class_name(HARNESS_CLASS);
        doc.body().unwrap().append_child(&frame).unwrap();
        let inner: web_sys::Document = js_sys::Reflect::get(&frame, &"contentDocument".into())
            .unwrap()
            .unchecked_into();
        let inner_window: web_sys::Window = js_sys::Reflect::get(&frame, &"contentWindow".into())
            .unwrap()
            .unchecked_into();
        add_sheet(&inner, STYLE_CSS);
        add_sheet(&inner, GLOBAL_CSS);
        inner.body().unwrap().set_inner_html(
            "<div><div class='meeting-status-bar'></div><div class='peer-toasts' id='under-bar'></div></div>\
             <div><div class='peer-toasts' id='alone'></div></div>",
        );
        let under_bar = inner.get_element_by_id("under-bar").unwrap();
        let alone = inner.get_element_by_id("alone").unwrap();
        let top_at = |width: u32, el: &web_sys::Element| {
            frame
                .set_attribute("style", &format!("width: {width}px; height: 400px"))
                .unwrap();
            computed_in(&inner_window, el, "top")
        };
        assert_eq!(top_at(800, &under_bar), "48px", "desktop with the bar");
        assert_eq!(top_at(800, &alone), "16px", "desktop without the bar");
        assert_eq!(top_at(600, &under_bar), "48px", "mobile with the bar");
        assert_eq!(
            top_at(400, &under_bar),
            "40px",
            "narrow with the shorter bar"
        );
        assert_eq!(top_at(600, &alone), "8px", "mobile without the bar");
        frame.remove();
    }
}
