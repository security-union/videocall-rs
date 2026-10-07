// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 2895: tells the user when the call runs on WebSocket although they
//! prefer WebTransport.

use crate::components::attendants::action_bar_announce_text;
use crate::context::TransportPreference;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::Cell;
use std::rc::Rc;

const TOAST_TITLE: &str = "Using WebSocket";
const TOAST_DETAIL: &str = "WebTransport, your preferred protocol, isn't working well here.";
pub const FALLBACK_TOAST_MS: u32 = 8_000;
pub const FALLBACK_NOTE: &str =
    "This call is using WebSocket because WebTransport isn't working well on this device or network.";

/// The transport the call is using, `None` while not connected.
#[derive(Clone, Copy)]
pub struct ActiveTransportCtx(pub Signal<Option<TransportPreference>>);

/// Maps `VideoCallClient::active_transport`'s labels.
pub fn parse_active_transport(raw: Option<&str>) -> Option<TransportPreference> {
    match raw? {
        "webtransport" => Some(TransportPreference::WebTransport),
        "websocket" => Some(TransportPreference::WebSocket),
        _ => None,
    }
}

/// `None` when the active transport is unknown. WebSocket on a deployment
/// that never offered WebTransport is not a fallback.
pub fn transport_fallback_active(
    preferred: TransportPreference,
    server_wt_enabled: bool,
    active: Option<TransportPreference>,
) -> Option<bool> {
    active.map(|active| {
        preferred == TransportPreference::WebTransport
            && server_wt_enabled
            && active == TransportPreference::WebSocket
    })
}

/// Returns `(in_fallback, show_toast)`.
pub fn next_transport_fallback(
    was_in_fallback: bool,
    preferred: TransportPreference,
    server_wt_enabled: bool,
    active: Option<TransportPreference>,
) -> (bool, bool) {
    match transport_fallback_active(preferred, server_wt_enabled, active) {
        Some(now) => (now, now && !was_in_fallback),
        None => (was_in_fallback, false),
    }
}

#[derive(Clone, Copy)]
pub struct TransportFallback {
    pub toast: Signal<Option<u32>>,
    pub active: Signal<Option<TransportPreference>>,
}

pub fn use_transport_fallback<R>(
    call_start_time: Signal<Option<f64>>,
    connection_error: Signal<Option<String>>,
    preferred: Signal<TransportPreference>,
    server_wt_enabled: fn() -> bool,
    toast_ms: u32,
    make_reader: impl FnOnce() -> R,
) -> TransportFallback
where
    R: Fn() -> Option<&'static str> + 'static,
{
    let read_active = use_hook(|| Rc::new(make_reader()));
    let in_fallback = use_hook(|| Rc::new(Cell::new(false)));
    let mut toast = use_signal(|| None::<u32>);
    let mut timer = use_signal(|| None::<Timeout>);
    let mut active = use_signal(|| None::<TransportPreference>);

    use_effect(move || {
        let started = call_start_time.read().is_some();
        let connected = started && connection_error.read().is_none();
        let preferred = preferred();
        let observed = if connected {
            parse_active_transport(read_active())
        } else {
            None
        };
        if *active.peek() != observed {
            active.set(observed);
        }
        if !started {
            in_fallback.set(false);
            if toast.peek().is_some() {
                toast.set(None);
                timer.set(None);
            }
            return;
        }
        let (now, show) =
            next_transport_fallback(in_fallback.get(), preferred, server_wt_enabled(), observed);
        in_fallback.set(now);
        if show {
            let seq = toast.peek().map_or(1, |seq| seq.wrapping_add(1));
            toast.set(Some(seq));
            timer.set(Some(Timeout::new(toast_ms, move || {
                if let Ok(mut slot) = toast.try_write() {
                    *slot = None;
                }
            })));
        }
    });

    TransportFallback { toast, active }
}

#[component]
pub fn TransportFallbackNotice(toast: Signal<Option<u32>>) -> Element {
    let current = toast();
    let status_text = current.map_or_else(String::new, |seq| {
        action_bar_announce_text(&format!("{TOAST_TITLE}. {TOAST_DETAIL}"), seq)
    });
    rsx! {
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": "transport-fallback-status",
            "{status_text}"
        }
        if let Some(seq) = current {
            div {
                key: "{seq}",
                class: "peer-toast toast-left",
                "data-testid": "transport-fallback-toast",
                span { class: "toast-icon",
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
                        circle { cx: "12", cy: "12", r: "10" }
                        line {
                            x1: "12",
                            y1: "8",
                            x2: "12",
                            y2: "12",
                        }
                        line {
                            x1: "12",
                            y1: "16",
                            x2: "12.01",
                            y2: "16",
                        }
                    }
                }
                span { class: "toast-text",
                    span { class: "toast-name", "{TOAST_TITLE}" }
                    br {}
                    span { class: "toast-action", "{TOAST_DETAIL}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TransportPreference::{WebSocket, WebTransport};

    fn run(preferred: TransportPreference, server_wt: bool, steps: &[Option<&str>]) -> Vec<bool> {
        let mut in_fallback = false;
        steps
            .iter()
            .map(|raw| {
                let (now, show) = next_transport_fallback(
                    in_fallback,
                    preferred,
                    server_wt,
                    parse_active_transport(*raw),
                );
                in_fallback = now;
                show
            })
            .collect()
    }

    #[test]
    fn fallback_needs_a_webtransport_preference_an_offered_webtransport_and_websocket() {
        assert_eq!(
            transport_fallback_active(WebTransport, true, Some(WebSocket)),
            Some(true)
        );
        assert_eq!(
            transport_fallback_active(WebTransport, true, Some(WebTransport)),
            Some(false)
        );
        assert_eq!(
            transport_fallback_active(WebTransport, false, Some(WebSocket)),
            Some(false)
        );
        assert_eq!(
            transport_fallback_active(WebSocket, true, Some(WebSocket)),
            Some(false)
        );
        assert_eq!(transport_fallback_active(WebTransport, true, None), None);
    }

    #[test]
    fn active_transport_parses_only_the_client_labels() {
        assert_eq!(
            parse_active_transport(Some("webtransport")),
            Some(WebTransport)
        );
        assert_eq!(parse_active_transport(Some("websocket")), Some(WebSocket));
        assert_eq!(parse_active_transport(Some("auto")), None);
        assert_eq!(parse_active_transport(None), None);
    }

    #[test]
    fn websocket_at_join_toasts_once() {
        assert_eq!(run(WebTransport, true, &[Some("websocket")]), [true]);
    }

    #[test]
    fn a_reconnect_back_onto_websocket_does_not_re_toast() {
        assert_eq!(
            run(
                WebTransport,
                true,
                &[Some("websocket"), None, Some("websocket")]
            ),
            [true, false, false]
        );
    }

    #[test]
    fn only_an_observed_webtransport_connection_re_arms_the_toast() {
        assert_eq!(
            run(
                WebTransport,
                true,
                &[
                    Some("webtransport"),
                    Some("websocket"),
                    Some("webtransport"),
                    Some("websocket"),
                ]
            ),
            [false, true, false, true]
        );
    }

    #[test]
    fn a_deployment_without_webtransport_never_toasts() {
        assert_eq!(
            run(WebTransport, false, &[Some("websocket"), Some("websocket")]),
            [false, false]
        );
    }

    #[test]
    fn a_websocket_preference_never_toasts() {
        assert_eq!(
            run(WebSocket, true, &[Some("websocket"), Some("websocket")]),
            [false, false]
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    const SHORT_TOAST_MS: u32 = 250;

    fn harness(toast_ms: u32) -> Element {
        let mut call_start_time = use_signal(|| None::<f64>);
        let mut connection_error = use_signal(|| None::<String>);
        let preferred = use_signal(|| TransportPreference::WebTransport);
        let mut reported = use_signal(|| None::<&'static str>);
        let fallback = use_transport_fallback(
            call_start_time,
            connection_error,
            preferred,
            || true,
            toast_ms,
            || move || *reported.peek(),
        );
        let mut connect = move |raw: Option<&'static str>| {
            reported.set(raw);
            connection_error.set(None);
            let next = call_start_time.peek().unwrap_or(0.0) + 1.0;
            call_start_time.set(Some(next));
        };
        let toast = fallback.toast;
        let active = fallback.active;
        rsx! {
            div {
                id: "probe",
                "data-toast": "{toast():?}",
                "data-active": "{active():?}",
            }
            button { id: "ws", onclick: move |_| connect(Some("websocket")) }
            button { id: "wt", onclick: move |_| connect(Some("webtransport")) }
            button { id: "unknown", onclick: move |_| connect(None) }
            button {
                id: "lost",
                onclick: move |_| connection_error.set(Some("lost".to_string())),
            }
            button { id: "hangup", onclick: move |_| call_start_time.set(None) }
            TransportFallbackNotice { toast }
        }
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        harness(FALLBACK_TOAST_MS)
    }

    #[allow(non_snake_case)]
    fn ShortHarness() -> Element {
        harness(SHORT_TOAST_MS)
    }

    fn launch(app: fn() -> Element) -> web_sys::Element {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(app),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        mount
    }

    fn click(mount: &web_sys::Element, id: &str) {
        mount
            .query_selector(&format!("#{id}"))
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
    }

    fn probe(mount: &web_sys::Element, name: &str) -> String {
        mount
            .query_selector("#probe")
            .unwrap()
            .and_then(|el| el.get_attribute(name))
            .unwrap_or_default()
    }

    fn text_of(mount: &web_sys::Element, test_id: &str) -> Option<String> {
        mount
            .query_selector(&format!("[data-testid='{test_id}']"))
            .unwrap()
            .map(|el| el.text_content().unwrap_or_default())
    }

    async fn settle() {
        TimeoutFuture::new(30).await;
    }

    #[wasm_bindgen_test]
    async fn the_toast_fires_on_entering_fallback_and_resets_on_hang_up() {
        let mount = launch(Harness);
        settle().await;
        assert_eq!(
            probe(&mount, "data-toast"),
            "None",
            "no toast before joining"
        );

        click(&mount, "ws");
        settle().await;
        assert_eq!(probe(&mount, "data-toast"), "Some(1)", "WebSocket at join");
        assert_eq!(probe(&mount, "data-active"), "Some(WebSocket)");
        let toast = text_of(&mount, "transport-fallback-toast").expect("toast rendered");
        assert!(
            toast.contains(TOAST_TITLE) && toast.contains(TOAST_DETAIL),
            "{toast}"
        );
        let status = text_of(&mount, "transport-fallback-status").unwrap_or_default();
        assert!(
            status.starts_with(&format!("{TOAST_TITLE}. {TOAST_DETAIL}")),
            "{status}"
        );

        click(&mount, "unknown");
        settle().await;
        click(&mount, "ws");
        settle().await;
        assert_eq!(
            probe(&mount, "data-toast"),
            "Some(1)",
            "a reconnect back onto WebSocket must not re-toast"
        );

        click(&mount, "lost");
        settle().await;
        assert_eq!(probe(&mount, "data-active"), "None", "not connected");
        assert_eq!(probe(&mount, "data-toast"), "Some(1)");

        click(&mount, "wt");
        settle().await;
        assert_eq!(probe(&mount, "data-active"), "Some(WebTransport)");
        click(&mount, "ws");
        settle().await;
        assert_eq!(
            probe(&mount, "data-toast"),
            "Some(2)",
            "WebTransport re-arms the toast"
        );

        click(&mount, "hangup");
        settle().await;
        assert_eq!(
            probe(&mount, "data-toast"),
            "None",
            "hang-up clears the toast"
        );
        assert_eq!(probe(&mount, "data-active"), "None");
        assert_eq!(text_of(&mount, "transport-fallback-toast"), None);

        click(&mount, "ws");
        settle().await;
        assert_eq!(
            probe(&mount, "data-toast"),
            "Some(1)",
            "a rejoin on WebSocket toasts again"
        );
        mount.remove();
    }

    #[wasm_bindgen_test]
    async fn the_toast_dismisses_itself_after_its_duration() {
        let mount = launch(ShortHarness);
        settle().await;
        click(&mount, "ws");
        settle().await;
        assert_eq!(probe(&mount, "data-toast"), "Some(1)");
        assert!(text_of(&mount, "transport-fallback-toast").is_some());

        let give_up = js_sys::Date::now() + 3_000.0;
        while probe(&mount, "data-toast") != "None" && js_sys::Date::now() < give_up {
            TimeoutFuture::new(25).await;
        }
        assert_eq!(
            probe(&mount, "data-toast"),
            "None",
            "the toast must clear once its duration elapses"
        );
        assert_eq!(text_of(&mount, "transport-fallback-toast"), None);
        mount.remove();
    }
}
