// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 2895: which transport the call uses, and whether it is the one the
//! user prefers.

use crate::context::TransportPreference;
use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub const ACTIVE_TRANSPORT_POLL_MS: u32 = 1_000;

/// Maps `VideoCallClient::active_transport`'s labels.
pub fn parse_active_transport(raw: Option<&str>) -> Option<TransportPreference> {
    match raw? {
        "webtransport" => Some(TransportPreference::WebTransport),
        "websocket" => Some(TransportPreference::WebSocket),
        _ => None,
    }
}

pub fn transport_fallback_active(
    preferred: TransportPreference,
    server_wt_enabled: bool,
    active: Option<TransportPreference>,
) -> Option<bool> {
    active.map(|active| server_wt_enabled && active != preferred)
}

/// The Settings › Network note for a call running on `active`.
pub fn fallback_note(active: TransportPreference) -> &'static str {
    match active {
        TransportPreference::WebSocket => {
            "This call is using WebSocket because WebTransport isn't working well on this device or network."
        }
        TransportPreference::WebTransport => {
            "This call is using WebTransport because WebSocket isn't working well on this device or network."
        }
    }
}

/// While `enabled`, reads the transport on every render and every `period_ms`,
/// re-rendering only when it changes. `None` while disabled.
pub fn use_polled_active_transport<R>(
    enabled: bool,
    period_ms: u32,
    make_reader: impl FnOnce() -> R,
) -> Option<TransportPreference>
where
    R: Fn() -> Option<&'static str> + 'static,
{
    let read = use_hook(|| Rc::new(make_reader()));
    let shown = use_hook(|| Rc::new(Cell::new(None::<TransportPreference>)));
    let ticker = use_hook(|| Rc::new(RefCell::new(None::<Interval>)));
    let mut changed = use_signal(|| 0u32);
    let _ = changed();

    let current = if enabled {
        parse_active_transport(read())
    } else {
        None
    };
    shown.set(current);
    let running = ticker.borrow().is_some();
    if enabled != running {
        *ticker.borrow_mut() = enabled.then(|| {
            let read = read.clone();
            let shown = shown.clone();
            Interval::new(period_ms, move || {
                if parse_active_transport(read()) != shown.get() {
                    if let Ok(mut seq) = changed.try_write() {
                        *seq = seq.wrapping_add(1);
                    }
                }
            })
        });
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use TransportPreference::{WebSocket, WebTransport};

    #[test]
    fn either_preference_can_fall_back_when_webtransport_is_offered() {
        assert_eq!(
            transport_fallback_active(WebTransport, true, Some(WebSocket)),
            Some(true)
        );
        assert_eq!(
            transport_fallback_active(WebSocket, true, Some(WebTransport)),
            Some(true)
        );
        assert_eq!(
            transport_fallback_active(WebTransport, true, Some(WebTransport)),
            Some(false)
        );
        assert_eq!(
            transport_fallback_active(WebSocket, true, Some(WebSocket)),
            Some(false)
        );
        assert_eq!(
            transport_fallback_active(WebTransport, false, Some(WebSocket)),
            Some(false)
        );
        assert_eq!(transport_fallback_active(WebTransport, true, None), None);
    }

    #[test]
    fn the_note_names_the_protocol_in_use() {
        assert!(fallback_note(WebSocket).starts_with("This call is using WebSocket"));
        assert!(fallback_note(WebTransport).starts_with("This call is using WebTransport"));
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
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    const SHORT_POLL_MS: u32 = 50;

    thread_local! {
        static CLIENT_REPORTS: Cell<Option<&'static str>> = const { Cell::new(None) };
        static CLIENT_READS: Cell<u32> = const { Cell::new(0) };
    }

    fn client_reports(raw: Option<&'static str>) {
        CLIENT_REPORTS.with(|cell| cell.set(raw));
    }

    fn client_reads() -> u32 {
        CLIENT_READS.with(Cell::get)
    }

    #[allow(non_snake_case)]
    fn PollHarness() -> Element {
        let mut enabled = use_signal(|| false);
        let active = use_polled_active_transport(enabled(), SHORT_POLL_MS, || {
            || {
                CLIENT_READS.with(|reads| reads.set(reads.get() + 1));
                CLIENT_REPORTS.with(Cell::get)
            }
        });
        rsx! {
            div { id: "probe", "data-active": "{active:?}" }
            button { id: "open", onclick: move |_| enabled.set(true) }
            button { id: "close", onclick: move |_| enabled.set(false) }
        }
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

    async fn settle() {
        TimeoutFuture::new(30).await;
    }

    async fn active_within(mount: &web_sys::Element, expected: &str) -> String {
        let give_up = js_sys::Date::now() + 2_000.0;
        while probe(mount, "data-active") != expected && js_sys::Date::now() < give_up {
            TimeoutFuture::new(10).await;
        }
        probe(mount, "data-active")
    }

    #[wasm_bindgen_test]
    async fn the_polled_transport_follows_the_client_only_while_enabled() {
        client_reports(Some("webtransport"));
        let mount = launch(PollHarness);
        TimeoutFuture::new(SHORT_POLL_MS * 4).await;
        let disabled = probe(&mount, "data-active");
        let disabled_reads = client_reads();

        click(&mount, "open");
        settle().await;
        let on_open = probe(&mount, "data-active");

        client_reports(None);
        let reconnecting = active_within(&mount, "None").await;

        client_reports(Some("websocket"));
        let reconnected = active_within(&mount, "Some(WebSocket)").await;

        click(&mount, "close");
        settle().await;
        let closed = probe(&mount, "data-active");
        let reads_at_close = client_reads();
        TimeoutFuture::new(SHORT_POLL_MS * 4).await;
        let reads_after_close = client_reads();
        mount.remove();

        assert_eq!(disabled, "None", "a disabled hook reports nothing");
        assert_eq!(disabled_reads, 0, "a disabled hook never reads the client");
        assert_eq!(on_open, "Some(WebTransport)");
        assert_eq!(
            reconnecting, "None",
            "a client-internal reconnect must clear the value with no other signal"
        );
        assert_eq!(reconnected, "Some(WebSocket)");
        assert_eq!(closed, "None");
        assert_eq!(
            reads_after_close, reads_at_close,
            "no polling once disabled"
        );
    }
}
