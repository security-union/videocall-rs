/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Host Controls component - allows admitted participants to admit/reject waiting participants.
//!
//! Instead of polling every 3 seconds, this component receives a
//! `waiting_room_version` counter from the parent that is incremented
//! whenever the `on_waiting_room_updated` push event fires on the main
//! `VideoCallClient`. The `use_effect` reacts to changes in this counter
//! and fetches the waiting room list once per notification.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::meeting_api::JoinError;
use dioxus::prelude::*;
use videocall_meeting_types::responses::ParticipantStatusResponse;
use wasm_bindgen::JsCast;
use web_sys::HtmlAudioElement;

/// Polling interval in milliseconds for the safety-net timer.
const POLL_INTERVAL_MS: i32 = 10_000;

/// Log one error per this many consecutive unauthenticated polls (~1/minute at
/// [`POLL_INTERVAL_MS`]).
const POLL_AUTH_ERROR_EVERY: u32 = 6;

pub type WaitingParticipant = ParticipantStatusResponse;

/// Adds each waiting user id to `announced`; true if any was not already
/// there. An id leaves the set only via [`AnnouncedIds::forget`].
fn announce_arrivals<'a>(
    announced: &mut HashSet<String>,
    waiting: impl IntoIterator<Item = &'a str>,
) -> bool {
    let mut arrived = false;
    for user_id in waiting {
        arrived |= announced.insert(user_id.to_owned());
    }
    arrived
}

/// Waiting user ids already knocked for in this mount.
#[derive(Clone, Default)]
struct AnnouncedIds(Rc<RefCell<HashSet<String>>>);

impl AnnouncedIds {
    fn knock_for_arrivals(&self, waiting: &[WaitingParticipant]) {
        let arrived = announce_arrivals(
            &mut self.0.borrow_mut(),
            waiting.iter().map(|p| p.user_id.as_str()),
        );
        if arrived {
            play_knock_sound();
        }
    }

    fn forget<'a>(&self, user_ids: impl IntoIterator<Item = &'a str>) {
        let mut announced = self.0.borrow_mut();
        for user_id in user_ids {
            announced.remove(user_id);
        }
    }
}

#[component]
pub fn HostControls(
    meeting_id: String,
    is_admitted: bool,
    /// Counter incremented by the parent whenever a `on_waiting_room_updated`
    /// push event is received. The component fetches the waiting list each
    /// time this value changes.
    waiting_room_version: Signal<u64>,
) -> Element {
    let mut waiting = use_signal(Vec::<WaitingParticipant>::new);
    let mut error = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| true);
    let announced: AnnouncedIds = use_hook(AnnouncedIds::default);

    let fetch_waiting_list = {
        let meeting_id = meeting_id.clone();
        move || {
            if !is_admitted {
                return;
            }
            let meeting_id = meeting_id.clone();
            spawn(async move {
                match fetch_waiting(&meeting_id).await {
                    Ok(w) => {
                        waiting.set(w);
                        error.set(None);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch waiting room: {e}");
                        error.set(Some(e.to_string()));
                    }
                }
            });
        }
    };

    // Fetch on mount and whenever waiting_room_version changes (push notification).
    {
        let meeting_id = meeting_id.clone();
        let announced = announced.clone();
        use_effect(move || {
            // Read the version so Dioxus tracks it as a reactive dependency.
            let _version = waiting_room_version();
            if !is_admitted {
                return;
            }

            let meeting_id = meeting_id.clone();
            let announced = announced.clone();
            spawn(async move {
                match fetch_waiting(&meeting_id).await {
                    Ok(w) => {
                        announced.knock_for_arrivals(&w);
                        waiting.set(w);
                        error.set(None);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch waiting room: {e}");
                        error.set(Some(e.to_string()));
                    }
                }
            });
        });
    }

    // Polling safety net: fetch the waiting list every POLL_INTERVAL_MS
    // regardless of whether push notifications are working. This catches
    // attendees who joined the waiting room before the host's observer
    // WebSocket was connected (NATS event lost).
    let poll_interval_id: Rc<Cell<i32>> = use_hook(|| Rc::new(Cell::new(-1)));
    let poll_auth_failures: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));
    {
        let meeting_id = meeting_id.clone();
        let poll_interval_id = poll_interval_id.clone();
        let poll_auth_failures = poll_auth_failures.clone();
        let announced = announced.clone();
        use_effect(move || {
            if !is_admitted {
                return;
            }
            let window = match web_sys::window() {
                Some(w) => w,
                None => return,
            };

            log::info!("HostControls: starting polling safety net (every {POLL_INTERVAL_MS}ms)");

            let meeting_id = meeting_id.clone();
            let poll_auth_failures = poll_auth_failures.clone();
            let announced = announced.clone();
            let poll_closure = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                let meeting_id = meeting_id.clone();
                let poll_auth_failures = poll_auth_failures.clone();
                let announced = announced.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match fetch_waiting(&meeting_id).await {
                        Ok(w) => {
                            announced.knock_for_arrivals(&w);
                            waiting.set(w);
                            error.set(None);
                            poll_auth_failures.set(0);
                        }
                        Err(e) => {
                            report_poll_failure(&poll_auth_failures, &e);
                        }
                    }
                });
            });

            let interval_id = window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    poll_closure.as_ref().unchecked_ref(),
                    POLL_INTERVAL_MS,
                )
                .unwrap_or(-1);

            poll_closure.forget();
            poll_interval_id.set(interval_id);
        });
    }

    // Clean up the polling interval when the component unmounts.
    {
        let poll_interval_id = poll_interval_id.clone();
        use_drop(move || {
            let id = poll_interval_id.get();
            if id >= 0 {
                if let Some(window) = web_sys::window() {
                    window.clear_interval_with_handle(id);
                    log::debug!("HostControls: cleared polling interval {id} on unmount");
                }
            }
        });
    }

    if !is_admitted || waiting().is_empty() {
        return rsx! {};
    }

    let show_admit_all = waiting().len() > 1;

    let on_admit_all = {
        let meeting_id = meeting_id.clone();
        let fetch_waiting_list = fetch_waiting_list.clone();
        let announced = announced.clone();
        move |_| {
            let admitted: Vec<String> = waiting.peek().iter().map(|p| p.user_id.clone()).collect();
            waiting.write().clear();
            let meeting_id = meeting_id.clone();
            let fetch_waiting_list = fetch_waiting_list.clone();
            let announced = announced.clone();
            spawn(async move {
                let result = admit_all_participants(&meeting_id).await;
                announced.forget(admitted.iter().map(String::as_str));
                if let Err(e) = result {
                    error.set(Some(e));
                }
                fetch_waiting_list();
            });
        }
    };

    let decide = {
        let meeting_id = meeting_id.clone();
        let fetch_waiting_list = fetch_waiting_list.clone();
        let announced = announced.clone();
        move |user_id: String, admit: bool| {
            let meeting_id = meeting_id.clone();
            let fetch = fetch_waiting_list.clone();
            let announced = announced.clone();
            move |_: MouseEvent| {
                waiting.write().retain(|p| p.user_id != user_id);
                let user_id = user_id.clone();
                let meeting_id = meeting_id.clone();
                let fetch = fetch.clone();
                let announced = announced.clone();
                spawn(async move {
                    let result = if admit {
                        admit_participant(&meeting_id, &user_id).await
                    } else {
                        reject_participant(&meeting_id, &user_id).await
                    };
                    announced.forget([user_id.as_str()]);
                    if let Err(e) = result {
                        error.set(Some(e));
                    }
                    fetch();
                });
            }
        }
    };

    rsx! {
        div { class: "host-controls-container",
            button { class: "host-controls-toggle", onclick: move |_| expanded.set(!expanded()),
                span { class: "waiting-badge", "{waiting().len()}" }
                span { "Waiting to join" }
                svg {
                    class: if expanded() { "chevron-icon expanded" } else { "chevron-icon" },
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "16", height: "16",
                    view_box: "0 0 24 24",
                    fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
            }

            if expanded() {
                div { class: "host-controls-list",
                    if show_admit_all {
                        div { class: "admit-all-container",
                            button { class: "btn-admit-all", onclick: on_admit_all,
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                    polyline { points: "20 6 9 17 4 12" }
                                }
                                "Admit all ({waiting().len()})"
                            }
                        }
                    }
                    for (user_id, label, guest_badge) in waiting().iter().map(waiting_row) {
                        div { key: "{user_id}", class: "waiting-participant",
                            div { class: "participant-info",
                                div { class: "participant-name",
                                    "{label}"
                                    if guest_badge {
                                        span { class: "guest-badge", "Guest" }
                                    }
                                }
                            }
                            div { class: "participant-actions",
                                button {
                                    class: "btn-admit",
                                    title: "Admit",
                                    aria_label: "Admit {label}",
                                    onclick: decide(user_id.clone(), true),
                                    svg {
                                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                        polyline { points: "20 6 9 17 4 12" }
                                    }
                                }
                                button {
                                    class: "btn-reject",
                                    title: "Reject",
                                    aria_label: "Reject {label}",
                                    onclick: decide(user_id.clone(), false),
                                    svg {
                                        xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                        stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                        line { x1: "18", y1: "6", x2: "6", y2: "18" }
                                        line { x1: "6", y1: "6", x2: "18", y2: "18" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `(user_id, label, guest badge)`: a blank display name shows the user id.
fn waiting_row(participant: &WaitingParticipant) -> (String, String, bool) {
    match participant.display_name.as_deref() {
        Some(name) if !name.trim().is_empty() => (
            participant.user_id.clone(),
            name.to_string(),
            participant.is_guest,
        ),
        _ => (
            participant.user_id.clone(),
            participant.user_id.clone(),
            false,
        ),
    }
}

fn play_knock_sound() {
    if let Ok(audio) = HtmlAudioElement::new_with_src("/assets/knock.wav") {
        audio.set_volume(0.5);
        if let Err(e) = audio.play() {
            log::warn!("Failed to play knock sound: {e:?}");
        }
    }
}

/// Whether the `n`th consecutive unauthenticated poll gets an error line.
/// Periodic, not first-only: a long outage must never fall permanently silent.
fn should_report_auth_failure(n: u32) -> bool {
    n % POLL_AUTH_ERROR_EVERY == 1
}

/// Report a poll failure. An unauthenticated one means the waiting list has
/// stopped updating, so it is an error, rate-limited by
/// [`should_report_auth_failure`] and reset by the next successful poll.
fn report_poll_failure(consecutive_auth_failures: &Cell<u32>, e: &JoinError) {
    if !matches!(e, JoinError::NotAuthenticated) {
        log::warn!("HostControls poll: failed to fetch waiting room: {e}");
        return;
    }
    let n = consecutive_auth_failures.get().saturating_add(1);
    consecutive_auth_failures.set(n);
    if should_report_auth_failure(n) {
        log::error!(
            "HostControls poll: waiting room unauthenticated ({n} consecutive); \
             host controls are not updating: {e}"
        );
    }
}

async fn fetch_waiting(meeting_id: &str) -> Result<Vec<WaitingParticipant>, JoinError> {
    crate::meeting_api::get_waiting_room(meeting_id).await
}

async fn admit_participant(meeting_id: &str, user_id: &str) -> Result<(), String> {
    crate::meeting_api::admit_participant(meeting_id, user_id)
        .await
        .map_err(|e| format!("{e}"))
}

async fn reject_participant(meeting_id: &str, user_id: &str) -> Result<(), String> {
    crate::meeting_api::reject_participant(meeting_id, user_id)
        .await
        .map_err(|e| format!("{e}"))
}

async fn admit_all_participants(meeting_id: &str) -> Result<(), String> {
    crate::meeting_api::admit_all(meeting_id)
        .await
        .map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod poll_failure_tests {
    use super::*;

    /// Issue #2291: 77 consecutive unauthenticated polls were rendered as warn
    /// noise.
    #[test]
    fn auth_failure_reporting_is_periodic_and_never_silent() {
        assert!(should_report_auth_failure(1), "first failure must report");
        let reported: Vec<u32> = (1..=80)
            .filter(|n| should_report_auth_failure(*n))
            .collect();
        assert_eq!(
            reported,
            vec![1, 7, 13, 19, 25, 31, 37, 43, 49, 55, 61, 67, 73, 79]
        );
    }

    #[test]
    fn only_unauthenticated_failures_advance_the_counter() {
        let counter = Cell::new(0u32);
        report_poll_failure(&counter, &JoinError::NotFound("gone".into()));
        assert_eq!(counter.get(), 0);
        report_poll_failure(&counter, &JoinError::NotAuthenticated);
        report_poll_failure(&counter, &JoinError::NotAuthenticated);
        assert_eq!(counter.get(), 2);
    }
}

#[cfg(test)]
mod knock_tests {
    use super::*;

    fn arrivals(ids: &AnnouncedIds, waiting: &[&str]) -> bool {
        announce_arrivals(&mut ids.0.borrow_mut(), waiting.iter().copied())
    }

    #[test]
    fn a_waiter_who_drops_off_the_list_and_returns_does_not_knock_again() {
        let ids = AnnouncedIds::default();
        assert!(arrivals(&ids, &["alice"]));
        assert!(!arrivals(&ids, &[]));
        assert!(!arrivals(&ids, &["alice"]));
    }

    #[test]
    fn a_new_arrival_knocks_even_when_the_list_length_is_unchanged() {
        let ids = AnnouncedIds::default();
        assert!(arrivals(&ids, &["alice"]));
        assert!(arrivals(&ids, &["bob"]));
        assert!(!arrivals(&ids, &["alice", "bob"]));
    }

    #[test]
    fn a_waiter_the_host_admitted_or_rejected_knocks_again_on_return() {
        let ids = AnnouncedIds::default();
        assert!(arrivals(&ids, &["alice", "bob"]));
        ids.forget(["alice"]);
        assert!(arrivals(&ids, &["alice", "bob"]));
        assert!(!arrivals(&ids, &["bob"]));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    thread_local! {
        static VERSION: RefCell<Option<Signal<u64>>> = const { RefCell::new(None) };
        static MOUNTED: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn Harness() -> Element {
        let version = use_signal(|| 0u64);
        let mounted = use_signal(|| true);
        use_hook(move || {
            VERSION.with(|v| *v.borrow_mut() = Some(version));
            MOUNTED.with(|m| *m.borrow_mut() = Some(mounted));
        });
        rsx! {
            if mounted() {
                HostControls {
                    meeting_id: "m-knock".to_string(),
                    is_admitted: true,
                    waiting_room_version: version,
                }
            }
        }
    }

    fn install_mocks() {
        js_sys::eval(
            r#"
            window.__APP_CONFIG = Object.freeze({
                apiBaseUrl: 'http://test:8080', wsUrl: 'ws://test:8080',
                webTransportHost: 'https://test:4433', oauthEnabled: 'false',
                e2eeEnabled: 'false', webTransportEnabled: 'false', firefoxEnabled: 'false',
                usersAllowedToStream: '', serverElectionPeriodMs: 2000, vadThreshold: 0.02
            });
            window.__knocks = 0;
            window.__waitingFetches = 0;
            window.__waitingIds = [];
            window.__original_play = window.__original_play || HTMLMediaElement.prototype.play;
            HTMLMediaElement.prototype.play = function () {
                window.__knocks += 1;
                return Promise.resolve();
            };
            window.__decisions = [];
            window.__admitFails = false;
            window.__original_fetch = window.__original_fetch || window.fetch;
            window.fetch = function (input, init) {
                var url = typeof input === 'string' ? input : input.url;
                var respond = function (status, body) {
                    var resp = new Response(JSON.stringify({ success: status === 200, result: body }),
                        { status: status, headers: { 'Content-Type': 'application/json' } });
                    Object.defineProperty(resp, 'url', { value: url });
                    return resp;
                };
                var row = function (id, status) {
                    return { user_id: id, display_name: id, status: status, is_host: false, joined_at: 0 };
                };
                if (url.endsWith('/waiting')) {
                    window.__waitingFetches += 1;
                    return Promise.resolve(respond(200, { meeting_id: 'm-knock',
                        waiting: window.__waitingIds.map(function (id) { return row(id, 'waiting'); }) }));
                }
                if (url.endsWith('/admit-all')) {
                    window.__decisions.push('admit-all');
                    return Promise.resolve(respond(200, { admitted_count: 0, admitted: [] }));
                }
                if (url.endsWith('/admit') || url.endsWith('/reject')) {
                    var verb = url.endsWith('/admit') ? 'admit' : 'reject';
                    return input.text().then(function (text) {
                        var id = JSON.parse(text).user_id;
                        window.__decisions.push(verb + ':' + id);
                        if (verb === 'admit' && window.__admitFails) {
                            return respond(404, { code: 'PARTICIPANT_NOT_FOUND', message: 'gone' });
                        }
                        return respond(200, row(id, verb === 'admit' ? 'admitted' : 'rejected'));
                    });
                }
                return window.__original_fetch(input, init);
            };
            "#,
        )
        .unwrap();
        crate::constants::reset_config_cache_for_test();
    }

    fn decisions() -> Vec<String> {
        let value = js_sys::Reflect::get(&gloo_utils::window(), &"__decisions".into()).unwrap();
        js_sys::Array::from(&value)
            .iter()
            .filter_map(|v| v.as_string())
            .collect()
    }

    fn button(root: &web_sys::Element, label: &str) -> web_sys::HtmlElement {
        root.query_selector(&format!("button[aria-label='{label}']"))
            .unwrap()
            .unwrap_or_else(|| panic!("no button labelled {label:?}"))
            .unchecked_into()
    }

    /// Clicks `target`, with the server then listing `after`, and waits for
    /// the refetch that follows the decision.
    async fn decide_on(target: &web_sys::HtmlElement, after: &[&str]) {
        set_waiting(after);
        let fetches = js_number("__waitingFetches");
        target.click();
        wait_until("the refetch after the decision", || {
            js_number("__waitingFetches") > fetches
        })
        .await;
    }

    async fn mount_with_waiting(ids: &[&str]) -> web_sys::Element {
        let stale = MOUNTED.with(|m| m.borrow_mut().take());
        if let Some(mut mounted) = stale {
            if mounted.try_peek().map(|v| *v).unwrap_or(false) {
                mounted.set(false);
                TimeoutFuture::new(50).await;
            }
        }
        install_mocks();
        set_waiting(ids);
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
        let expected: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        wait_until("the first list", || listed(&root) == expected).await;
        root
    }

    async fn unmount(root: web_sys::Element) {
        MOUNTED.with(|m| m.borrow().expect("mounted signal").set(false));
        TimeoutFuture::new(50).await;
        remove_mocks();
        root.remove();
    }

    fn remove_mocks() {
        js_sys::eval(
            r#"
            if (window.__original_fetch) { window.fetch = window.__original_fetch; delete window.__original_fetch; }
            if (window.__original_play) { HTMLMediaElement.prototype.play = window.__original_play; delete window.__original_play; }
            delete window.__APP_CONFIG;
            "#,
        )
        .unwrap();
        crate::constants::reset_config_cache_for_test();
    }

    fn js_number(name: &str) -> u32 {
        js_sys::Reflect::get(&gloo_utils::window(), &name.into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u32
    }

    fn set_waiting(ids: &[&str]) {
        let list = js_sys::Array::new();
        for id in ids {
            list.push(&(*id).into());
        }
        js_sys::Reflect::set(&gloo_utils::window(), &"__waitingIds".into(), &list).unwrap();
    }

    fn listed(root: &web_sys::Element) -> Vec<String> {
        let names = root
            .query_selector_all(".waiting-participant .participant-name")
            .unwrap();
        (0..names.length())
            .filter_map(|i| names.item(i).and_then(|n| n.text_content()))
            .collect()
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            TimeoutFuture::new(10).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn host_sees(root: &web_sys::Element, ids: &[&str]) {
        set_waiting(ids);
        VERSION.with(|v| {
            let mut version = v.borrow().expect("version signal");
            version += 1;
        });
        let expected: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        wait_until(&format!("the host list to show {ids:?}"), || {
            listed(root) == expected
        })
        .await;
    }

    #[wasm_bindgen_test]
    async fn the_knock_sounds_once_per_waiter_not_once_per_reappearance() {
        let root = mount_with_waiting(&["alice"]).await;
        assert_eq!(js_number("__knocks"), 1, "alice's first arrival knocks");

        host_sees(&root, &[]).await;
        host_sees(&root, &["alice"]).await;
        assert_eq!(
            js_number("__knocks"),
            1,
            "alice lapsing off the list and coming back is not a new arrival"
        );

        host_sees(&root, &["bob"]).await;
        assert_eq!(js_number("__knocks"), 2, "bob is a new arrival");

        host_sees(&root, &["alice", "bob"]).await;
        decide_on(&button(&root, "Reject alice"), &["bob"]).await;
        host_sees(&root, &["alice", "bob"]).await;
        assert_eq!(
            js_number("__knocks"),
            3,
            "alice re-queueing after the host rejected her knocks again"
        );

        unmount(root).await;
    }

    #[wasm_bindgen_test]
    async fn admit_a_failed_admit_and_admit_all_each_let_the_waiter_knock_again() {
        let root = mount_with_waiting(&["alice"]).await;
        assert_eq!(js_number("__knocks"), 1);

        decide_on(&button(&root, "Admit alice"), &[]).await;
        host_sees(&root, &["alice"]).await;
        assert_eq!(js_number("__knocks"), 2, "alice back after an admit");

        js_sys::eval("window.__admitFails = true;").unwrap();
        decide_on(&button(&root, "Admit alice"), &[]).await;
        host_sees(&root, &["alice"]).await;
        assert_eq!(
            js_number("__knocks"),
            3,
            "alice back after an admit that failed because her row had lapsed"
        );

        host_sees(&root, &["alice", "bob"]).await;
        assert_eq!(js_number("__knocks"), 4, "bob arrives");
        let admit_all = root
            .query_selector(".btn-admit-all")
            .unwrap()
            .expect("admit all")
            .unchecked_into::<web_sys::HtmlElement>();
        decide_on(&admit_all, &[]).await;
        host_sees(&root, &["alice", "bob"]).await;
        assert_eq!(js_number("__knocks"), 5, "both back after admit-all");
        assert_eq!(
            decisions(),
            ["admit:alice", "admit:alice", "admit-all"],
            "each click reached the server"
        );

        unmount(root).await;
    }

    #[wasm_bindgen_test]
    async fn a_row_keeps_its_waiter_when_an_earlier_waiter_leaves() {
        let root = mount_with_waiting(&["alice", "bob"]).await;
        let bobs_admit = root
            .query_selector_all(".waiting-participant .btn-admit")
            .unwrap()
            .item(1)
            .expect("bob's admit button")
            .unchecked_into::<web_sys::HtmlElement>();
        bobs_admit.focus().unwrap();

        host_sees(&root, &["bob"]).await;
        assert!(
            bobs_admit.is_connected(),
            "bob's row must survive alice leaving above it"
        );
        assert_eq!(
            bobs_admit.get_attribute("aria-label").as_deref(),
            Some("Admit bob")
        );
        decide_on(&bobs_admit, &[]).await;
        assert_eq!(decisions(), ["admit:bob"], "the held button admits bob");

        unmount(root).await;
    }
}
