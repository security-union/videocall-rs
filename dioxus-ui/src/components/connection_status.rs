// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 3044: connection-status card.

use crate::components::attendants::action_bar_announce_text;
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use gloo_timers::future::TimeoutFuture;
use videocall_client::{Callback as VcCallback, ElectionProgress, ELECTION_STARVED_GRACE_MS};

pub const CONNECTED_CARD_MS: u32 = 3_000;
const COUNTDOWN_DELAY_MS: f64 = 1_500.0;
const TICK_MS: u32 = 250;
const STILL_TRYING_ANNOUNCE_DELAY_MS: f64 = 1_000.0;
const STILL_TRYING: &str = "Still trying…";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConnectionStatus {
    pub progress: ElectionProgress,
    pub since_ms: f64,
    /// Bumped per event so a repeated phase is re-announced.
    pub nonce: u32,
}

fn protocol(webtransport: bool) -> &'static str {
    if webtransport {
        "WebTransport"
    } else {
        "WebSocket"
    }
}

/// `(title, detail)`; `detail` is empty for a success.
pub fn status_text(progress: ElectionProgress) -> (String, String) {
    match progress {
        ElectionProgress::Preferred { webtransport, .. } => (
            "Connecting to the meeting…".to_string(),
            format!("Using {}", protocol(webtransport)),
        ),
        ElectionProgress::Fallback { webtransport, .. } => (
            "Trying another connection…".to_string(),
            format!("Switching to {}", protocol(webtransport)),
        ),
        ElectionProgress::Connected { webtransport, .. } => (
            format!("Connected using {}", protocol(webtransport)),
            String::new(),
        ),
    }
}

/// What the live-region announcer renders for a card `elapsed_ms` old.
pub fn announcer_text(progress: ElectionProgress, elapsed_ms: f64, nonce: u32) -> String {
    let text = status_announcement(progress, still_trying_announced(progress, elapsed_ms));
    action_bar_announce_text(&text, nonce)
}

pub fn status_announcement(progress: ElectionProgress, still_trying: bool) -> String {
    let base = match status_text(progress) {
        (title, detail) if detail.is_empty() => title,
        (title, detail) => format!("{title} {detail}"),
    };
    if still_trying {
        format!("{base} {STILL_TRYING}")
    } else {
        base
    }
}

fn phase_timeout_ms(progress: ElectionProgress) -> Option<f64> {
    match progress {
        ElectionProgress::Preferred { timeout_ms, .. }
        | ElectionProgress::Fallback { timeout_ms, .. } => Some(timeout_ms as f64),
        ElectionProgress::Connected { .. } => None,
    }
}

/// `(seconds left, fraction elapsed)`.
pub fn countdown(progress: ElectionProgress, elapsed_ms: f64) -> Option<(u64, f64)> {
    let timeout = phase_timeout_ms(progress)?;
    if elapsed_ms < COUNTDOWN_DELAY_MS || timeout <= 0.0 {
        return None;
    }
    let left = timeout - elapsed_ms;
    (left > 0.0).then(|| ((left / 1000.0).ceil() as u64, elapsed_ms / timeout))
}

/// How long the card waits before its next re-render, `None` once nothing
/// it shows can change without a new event.
pub fn next_tick_ms(progress: ElectionProgress, elapsed_ms: f64) -> Option<f64> {
    let timeout = phase_timeout_ms(progress)?;
    if elapsed_ms >= timeout + ELECTION_STARVED_GRACE_MS as f64 {
        None
    } else if elapsed_ms >= timeout + STILL_TRYING_ANNOUNCE_DELAY_MS {
        Some(timeout + ELECTION_STARVED_GRACE_MS as f64 - elapsed_ms)
    } else if elapsed_ms >= timeout {
        Some(timeout + STILL_TRYING_ANNOUNCE_DELAY_MS - elapsed_ms)
    } else if elapsed_ms < COUNTDOWN_DELAY_MS {
        Some((COUNTDOWN_DELAY_MS - elapsed_ms).min(timeout - elapsed_ms))
    } else {
        Some(f64::from(TICK_MS).min(timeout - elapsed_ms))
    }
}

pub fn past_timeout(progress: ElectionProgress, elapsed_ms: f64) -> bool {
    phase_timeout_ms(progress).is_some_and(|timeout| elapsed_ms >= timeout)
}

pub fn still_trying_announced(progress: ElectionProgress, elapsed_ms: f64) -> bool {
    phase_timeout_ms(progress)
        .is_some_and(|timeout| elapsed_ms >= timeout + STILL_TRYING_ANNOUNCE_DELAY_MS)
}

pub fn card_visible(progress: ElectionProgress, elapsed_ms: f64) -> bool {
    match phase_timeout_ms(progress) {
        Some(timeout) => elapsed_ms < timeout + ELECTION_STARVED_GRACE_MS as f64,
        None => elapsed_ms < f64::from(CONNECTED_CARD_MS),
    }
}

fn phase_label(progress: ElectionProgress) -> &'static str {
    match progress {
        ElectionProgress::Preferred { .. } => "preferred",
        ElectionProgress::Fallback { .. } => "fallback",
        ElectionProgress::Connected { .. } => "connected",
    }
}

/// The client's `on_election_progress`: shows the card.
pub fn election_progress_callback(
    status: Signal<Option<ConnectionStatus>>,
) -> VcCallback<ElectionProgress> {
    VcCallback::from(move |progress: ElectionProgress| {
        let mut status = status;
        let since_ms = js_sys::Date::now();
        let Ok(mut slot) = status.try_write() else {
            return;
        };
        let nonce = slot.map_or(0, |s| s.nonce.wrapping_add(1));
        *slot = Some(ConnectionStatus {
            progress,
            since_ms,
            nonce,
        });
        drop(slot);
        if let ElectionProgress::Connected { .. } = progress {
            Timeout::new(CONNECTED_CARD_MS, move || clear_if_unchanged(status, nonce)).forget();
        }
    })
}

/// Clears the card unless a later event replaced it. A no-op once the signal
/// is gone (the user left the call).
fn clear_if_unchanged(mut status: Signal<Option<ConnectionStatus>>, nonce: u32) {
    let unchanged = status.try_peek().is_ok_and(|s| is_same_card(*s, nonce));
    if unchanged {
        if let Ok(mut slot) = status.try_write() {
            *slot = None;
        }
    }
}

fn is_same_card(slot: Option<ConnectionStatus>, nonce: u32) -> bool {
    slot.is_some_and(|s| s.nonce == nonce)
}

#[component]
pub fn ConnectionStatusCard(status: Signal<Option<ConnectionStatus>>) -> Element {
    let mut now = use_signal(js_sys::Date::now);
    use_resource(move || async move {
        let Some(current) = status() else {
            return;
        };
        now.set(js_sys::Date::now());
        while let Some(wait) =
            next_tick_ms(current.progress, js_sys::Date::now() - current.since_ms)
        {
            TimeoutFuture::new(wait.max(1.0) as u32).await;
            now.set(js_sys::Date::now());
        }
        now.set(js_sys::Date::now());
    });

    let mut announcement = use_signal(String::new);
    use_effect(move || {
        let text = status().map_or_else(String::new, |s| {
            let elapsed = now().max(s.since_ms) - s.since_ms;
            announcer_text(s.progress, elapsed, s.nonce)
        });
        if *announcement.peek() != text {
            announcement.set(text);
        }
    });

    let current = status();
    let visible = current.and_then(|current| {
        let elapsed = now().max(current.since_ms) - current.since_ms;
        card_visible(current.progress, elapsed).then_some((current, elapsed))
    });
    rsx! {
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": "connection-status-announcer",
            "{announcement}"
        }
        if let Some((current, elapsed)) = visible {
            ConnectionStatusBody { current, elapsed }
        }
    }
}

#[component]
fn ConnectionStatusBody(current: ConnectionStatus, elapsed: f64) -> Element {
    let (title, detail) = status_text(current.progress);
    let countdown = countdown(current.progress, elapsed);
    let still_trying = past_timeout(current.progress, elapsed);
    let connected = matches!(current.progress, ElectionProgress::Connected { .. });
    let class = if connected {
        "peer-toast toast-success connection-status-card"
    } else {
        "peer-toast toast-loading connection-status-card"
    };
    rsx! {
        div {
            class,
            "data-testid": "connection-status-card",
            "data-phase": phase_label(current.progress),
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
                    if connected {
                        polyline { points: "20 6 9 17 4 12" }
                    } else {
                        path { d: "M21 12a9 9 0 1 1-6.219-8.56" }
                    }
                }
            }
            span { class: "toast-text",
                span { class: "toast-name", "data-testid": "connection-status-title", "{title}" }
                if !detail.is_empty() {
                    br {}
                    span {
                        class: "toast-action",
                        "data-testid": "connection-status-detail",
                        "{detail}"
                    }
                }
                if still_trying {
                    span {
                        class: "connection-status-countdown",
                        "data-testid": "connection-status-waiting",
                        "aria-hidden": "true",
                        "{STILL_TRYING}"
                    }
                }
                if let Some((seconds, fraction)) = countdown {
                    span {
                        class: "connection-status-countdown",
                        "data-testid": "connection-status-countdown",
                        "aria-hidden": "true",
                        "{seconds}s"
                    }
                    div {
                        class: "connection-status-progress",
                        "aria-hidden": "true",
                        div {
                            class: "connection-status-progress-bar",
                            style: "transform: scaleX({fraction:.3})",
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: ElectionProgress = ElectionProgress::Preferred {
        webtransport: true,
        timeout_ms: 5_000,
    };
    const WS_FALLBACK: ElectionProgress = ElectionProgress::Fallback {
        webtransport: false,
        timeout_ms: 4_000,
    };

    #[test]
    fn each_phase_has_its_text() {
        let text = |p| status_text(p);
        assert_eq!(
            text(WT),
            (
                "Connecting to the meeting…".into(),
                "Using WebTransport".into()
            )
        );
        assert_eq!(
            text(WS_FALLBACK),
            (
                "Trying another connection…".into(),
                "Switching to WebSocket".into()
            )
        );
        assert_eq!(
            text(ElectionProgress::Connected { webtransport: true }),
            ("Connected using WebTransport".into(), String::new())
        );
        assert_eq!(
            text(ElectionProgress::Connected {
                webtransport: false,
            }),
            ("Connected using WebSocket".into(), String::new())
        );
        assert_eq!(
            status_announcement(WS_FALLBACK, false),
            "Trying another connection… Switching to WebSocket"
        );
    }

    #[test]
    fn running_past_the_timeout_is_announced_once_and_only_after_the_delay() {
        let announced = |elapsed| announcer_text(WS_FALLBACK, elapsed, 0);
        let plain = "Trying another connection… Switching to WebSocket";
        assert_eq!(
            announced(4_000.0),
            plain,
            "a normal fallback is not announced"
        );
        assert_eq!(announced(4_999.0), plain);
        assert_eq!(announced(5_000.0), format!("{plain} Still trying…"));
        assert_eq!(announced(5_000.0), announced(6_900.0));
        assert_eq!(
            announcer_text(
                ElectionProgress::Connected { webtransport: true },
                9_000.0,
                0
            ),
            "Connected using WebTransport"
        );
        assert_eq!(
            announcer_text(WS_FALLBACK, 0.0, 1),
            format!("{plain}\u{00A0}")
        );
    }

    #[test]
    fn a_stale_connected_timeout_does_not_clear_a_newer_card_from_the_same_millisecond() {
        let older = ConnectionStatus {
            progress: ElectionProgress::Connected { webtransport: true },
            since_ms: 1_000.0,
            nonce: 4,
        };
        let newer = ConnectionStatus {
            progress: WS_FALLBACK,
            since_ms: 1_000.0,
            nonce: 5,
        };
        assert!(is_same_card(Some(newer), newer.nonce));
        assert!(!is_same_card(Some(newer), older.nonce));
    }

    #[test]
    fn the_countdown_appears_after_one_and_a_half_seconds_and_counts_the_phase() {
        assert_eq!(countdown(WT, 1_499.0), None);
        assert_eq!(countdown(WT, 1_500.0), Some((4, 0.3)));
        assert_eq!(countdown(WS_FALLBACK, 3_900.0).map(|c| c.0), Some(1));
        assert_eq!(countdown(WS_FALLBACK, 4_000.0), None, "no frozen 0s");
        assert_eq!(
            countdown(ElectionProgress::Connected { webtransport: true }, 2_000.0),
            None
        );
    }

    #[test]
    fn the_card_stops_re_rendering_once_nothing_it_shows_can_change() {
        assert_eq!(
            next_tick_ms(WT, 0.0),
            Some(1_500.0),
            "no ticks before the countdown"
        );
        assert_eq!(next_tick_ms(WT, 2_000.0), Some(250.0));
        assert_eq!(
            next_tick_ms(WT, 4_900.0),
            Some(100.0),
            "wakes at the timeout"
        );
        assert_eq!(
            next_tick_ms(WT, 5_000.0),
            Some(1_000.0),
            "one wake at the announcement"
        );
        assert_eq!(
            next_tick_ms(WT, 6_000.0),
            Some(2_000.0),
            "one wake at the hide"
        );
        assert_eq!(next_tick_ms(WT, 7_900.0), Some(100.0));
        assert_eq!(next_tick_ms(WT, 8_000.0), None, "no timer left, no spin");
        assert_eq!(
            next_tick_ms(ElectionProgress::Connected { webtransport: true }, 0.0),
            None
        );
    }

    #[test]
    fn a_success_card_is_brief_and_a_phase_hides_after_its_timeout_and_the_starved_grace() {
        let connected = ElectionProgress::Connected { webtransport: true };
        assert!(card_visible(connected, 2_999.0));
        assert!(!card_visible(connected, 3_000.0));
        assert!(card_visible(WT, 5_000.0));
        assert!(card_visible(WT, 6_000.0));
        assert!(card_visible(WT, 7_999.0));
        assert!(!card_visible(WT, 8_000.0));
        assert!(!past_timeout(WT, 4_999.0));
        assert!(past_timeout(WT, 5_000.0));
        assert!(!past_timeout(
            ElectionProgress::Connected { webtransport: true },
            9_000.0
        ));
    }

    #[test]
    fn the_hide_point_is_the_clients_starved_grace() {
        assert_eq!(ELECTION_STARVED_GRACE_MS, 3_000);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_test::wasm_bindgen_test;

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let mut status = use_signal(|| None::<ConnectionStatus>);
        let mut show = move |progress, age_ms: f64| {
            status.set(Some(ConnectionStatus {
                progress,
                since_ms: js_sys::Date::now() - age_ms,
                nonce: 0,
            }));
        };
        rsx! {
            button {
                id: "fresh",
                onclick: move |_| show(
                    ElectionProgress::Preferred {
                        webtransport: true,
                        timeout_ms: 5_000,
                    },
                    0.0,
                ),
            }
            button {
                id: "slow",
                onclick: move |_| show(
                    ElectionProgress::Fallback {
                        webtransport: false,
                        timeout_ms: 5_000,
                    },
                    2_000.0,
                ),
            }
            button {
                id: "overdue",
                onclick: move |_| show(
                    ElectionProgress::Fallback {
                        webtransport: false,
                        timeout_ms: 5_000,
                    },
                    6_000.0,
                ),
            }
            button {
                id: "window",
                onclick: move |_| show(
                    ElectionProgress::Fallback {
                        webtransport: false,
                        timeout_ms: 5_000,
                    },
                    5_500.0,
                ),
            }
            button {
                id: "stale-clear",
                onclick: move |_| {
                    status.set(Some(ConnectionStatus {
                        progress: ElectionProgress::Preferred {
                            webtransport: true,
                            timeout_ms: 5_000,
                        },
                        since_ms: js_sys::Date::now(),
                        nonce: 7,
                    }));
                    clear_if_unchanged(status, 3);
                },
            }
            button {
                id: "current-clear",
                onclick: move |_| clear_if_unchanged(status, 7),
            }
            button {
                id: "expired",
                onclick: move |_| show(
                    ElectionProgress::Fallback {
                        webtransport: false,
                        timeout_ms: 5_000,
                    },
                    9_000.0,
                ),
            }
            button {
                id: "done",
                onclick: move |_| show(
                    ElectionProgress::Connected {
                        webtransport: false,
                    },
                    0.0,
                ),
            }
            ConnectionStatusCard { status }
        }
    }

    #[wasm_bindgen_test]
    async fn the_card_shows_each_phase_and_the_countdown_only_after_a_delay() {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let click = |id: &str| {
            mount
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap()
                .unchecked_into::<web_sys::HtmlElement>()
                .click();
        };
        let text = |test_id: &str| {
            mount
                .query_selector(&format!("[data-testid='{test_id}']"))
                .unwrap()
                .map(|el| el.text_content().unwrap_or_default())
        };
        TimeoutFuture::new(30).await;
        assert_eq!(text("connection-status-card"), None);

        click("fresh");
        TimeoutFuture::new(30).await;
        assert_eq!(
            text("connection-status-title").as_deref(),
            Some("Connecting to the meeting…")
        );
        assert_eq!(
            text("connection-status-detail").as_deref(),
            Some("Using WebTransport")
        );
        assert_eq!(text("connection-status-countdown"), None);
        assert_eq!(
            text("connection-status-announcer").as_deref(),
            Some("Connecting to the meeting… Using WebTransport")
        );

        click("slow");
        TimeoutFuture::new(300).await;
        assert_eq!(
            text("connection-status-detail").as_deref(),
            Some("Switching to WebSocket")
        );
        assert_eq!(text("connection-status-countdown").as_deref(), Some("3s"));

        click("overdue");
        TimeoutFuture::new(300).await;
        assert_eq!(
            text("connection-status-waiting").as_deref(),
            Some("Still trying…")
        );
        assert_eq!(text("connection-status-countdown"), None);
        assert!(text("connection-status-card").is_some());
        assert_eq!(
            text("connection-status-announcer").as_deref(),
            Some("Trying another connection… Switching to WebSocket Still trying…")
        );

        click("expired");
        TimeoutFuture::new(300).await;
        assert_eq!(text("connection-status-card"), None);

        click("done");
        TimeoutFuture::new(30).await;
        assert_eq!(
            text("connection-status-title").as_deref(),
            Some("Connected using WebSocket")
        );
        assert_eq!(text("connection-status-countdown"), None);
        mount.remove();
    }

    #[wasm_bindgen_test]
    async fn the_announcer_waits_a_second_past_the_timeout_while_the_card_already_shows_still_trying(
    ) {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let text = |test_id: &str| {
            mount
                .query_selector(&format!("[data-testid='{test_id}']"))
                .unwrap()
                .map(|el| el.text_content().unwrap_or_default())
        };
        TimeoutFuture::new(30).await;
        mount
            .query_selector("#window")
            .unwrap()
            .unwrap()
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
        TimeoutFuture::new(100).await;
        assert_eq!(
            text("connection-status-waiting").as_deref(),
            Some("Still trying…")
        );
        assert_eq!(
            text("connection-status-announcer").as_deref(),
            Some("Trying another connection… Switching to WebSocket")
        );
        TimeoutFuture::new(800).await;
        assert_eq!(
            text("connection-status-announcer").as_deref(),
            Some("Trying another connection… Switching to WebSocket Still trying…")
        );
        mount.remove();
    }

    #[wasm_bindgen_test]
    async fn a_stale_connected_timeout_clears_only_the_card_it_was_started_for() {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let click = |id: &str| {
            mount
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap()
                .unchecked_into::<web_sys::HtmlElement>()
                .click();
        };
        let card_shown = || {
            mount
                .query_selector("[data-testid='connection-status-card']")
                .unwrap()
                .is_some()
        };
        TimeoutFuture::new(30).await;
        click("stale-clear");
        TimeoutFuture::new(50).await;
        assert!(card_shown(), "a stale nonce cleared a newer card");
        click("current-clear");
        TimeoutFuture::new(50).await;
        assert!(!card_shown(), "the matching nonce did not clear its card");
        mount.remove();
    }

    #[allow(non_snake_case)]
    fn PresetHarness() -> Element {
        let status = use_signal(|| {
            Some(ConnectionStatus {
                progress: ElectionProgress::Preferred {
                    webtransport: true,
                    timeout_ms: 5_000,
                },
                since_ms: js_sys::Date::now(),
                nonce: 0,
            })
        });
        rsx! {
            ConnectionStatusCard { status }
        }
    }

    #[wasm_bindgen_test]
    async fn a_card_mounted_with_a_status_fills_the_announcer_after_it_is_in_the_document() {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        let watch = js_sys::Function::new_with_args(
            "mount",
            "const state = { filled: false };
             new MutationObserver((records) => {
               for (const r of records) {
                 const el = r.target.nodeType === 1 ? r.target : r.target.parentElement;
                 if (el && el.getAttribute('data-testid') === 'connection-status-announcer') {
                   state.filled = true;
                 }
               }
             }).observe(mount, { subtree: true, childList: true, characterData: true });
             return state;",
        );
        let state = watch.call1(&JsValue::NULL, &mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(PresetHarness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        TimeoutFuture::new(100).await;
        let announcer = mount
            .query_selector("[data-testid='connection-status-announcer']")
            .unwrap()
            .unwrap();
        assert_eq!(
            announcer.text_content().as_deref(),
            Some("Connecting to the meeting… Using WebTransport")
        );
        let filled = js_sys::Reflect::get(&state, &JsValue::from_str("filled")).unwrap();
        assert!(
            filled.is_truthy(),
            "the announcer was inserted already holding its text"
        );
        mount.remove();
    }

    #[wasm_bindgen_test]
    async fn the_announcer_node_survives_the_card_showing_and_hiding() {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(Harness),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        let click = |id: &str| {
            mount
                .query_selector(&format!("#{id}"))
                .unwrap()
                .unwrap()
                .unchecked_into::<web_sys::HtmlElement>()
                .click();
        };
        let announcer = || {
            mount
                .query_selector("[data-testid='connection-status-announcer']")
                .unwrap()
                .expect("the announcer is always rendered")
        };
        let card_shown = || {
            mount
                .query_selector("[data-testid='connection-status-card']")
                .unwrap()
                .is_some()
        };
        TimeoutFuture::new(30).await;
        assert!(!card_shown());
        let mark = JsValue::from_str("__announcer_mark");
        js_sys::Reflect::set(&announcer(), &mark, &JsValue::TRUE).unwrap();
        let survives = || {
            js_sys::Reflect::get(&announcer(), &mark)
                .map(|v| v.is_truthy())
                .unwrap_or(false)
        };

        click("fresh");
        TimeoutFuture::new(30).await;
        assert!(card_shown());
        assert!(survives(), "showing the card replaced the announcer node");

        click("expired");
        TimeoutFuture::new(300).await;
        assert!(!card_shown());
        assert!(survives(), "hiding the card replaced the announcer node");

        click("done");
        TimeoutFuture::new(30).await;
        assert!(card_shown());
        assert!(
            survives(),
            "re-showing the card replaced the announcer node"
        );
        mount.remove();
    }
}
