// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 3163: a new meeting id must rebuild the meeting tree under `MeetingPage`,
// and leaving meeting A that way must release what A held.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::Cell;

use dioxus::prelude::*;
use dioxus_ui::components::attendants::{use_stop_recording_on_unmount, RecordingStateCallback};
use dioxus_ui::components::waiting_room::WaitingRoom;
use dioxus_ui::context::{DisplayNameCtx, TransportPreference, TransportPreferenceCtx};
use dioxus_ui::pages::meeting::MeetingPage;
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, reset_test_browser_state,
    restore_fetch, yield_now,
};
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const NAME_INPUT: &str = "input.input-apple";

thread_local! {
    static NAMED: Cell<bool> = const { Cell::new(false) };
}

/// With no display name the page stops at the name form; with one it joins.
#[allow(non_snake_case)]
fn Harness() -> Element {
    let transport = use_signal(|| TransportPreference::WebSocket);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let display_name = use_signal(|| NAMED.with(Cell::get).then(|| "Alice".to_string()));
    use_context_provider(|| DisplayNameCtx(display_name));
    let mut id = use_signal(|| "meeting-a".to_string());
    rsx! {
        button { id: "jump", onclick: move |_| id.set("meeting-b".to_string()) }
        MeetingPage { id: id() }
    }
}

fn click(mount: &web_sys::Element, selector: &str) {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
}

fn name_input(mount: &web_sys::Element) -> web_sys::Element {
    mount
        .query_selector(NAME_INPUT)
        .unwrap()
        .expect("the display-name form should be rendered")
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
}

/// Answers every `/join` with `status` (after `delay_ms`) and records every
/// request as `"METHOD url"` in `__fetches`. Every `WebSocket` is a stub that
/// records its URL and whether it was closed in `__sockets`.
fn mock_network(status: &str, delay_ms: u32) {
    js_sys::eval(&format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__original_ws = window.__original_ws || window.WebSocket;
        window.__fetches = [];
        window.__sockets = [];
        window.__errors = [];
        if (!window.__errorsHooked) {{
            window.__errorsHooked = true;
            window.addEventListener('error', function(e) {{ window.__errors.push(String(e.message)); }});
            window.addEventListener('unhandledrejection', function(e) {{
                window.__errors.push(String(e.reason));
            }});
        }}
        window.fetch = function(input, init) {{
            var req = typeof input === 'string' ? new Request(input, init) : input;
            window.__fetches.push(req.method + ' ' + req.url);
            var join = /\/join$/.test(req.url);
            var id = (req.url.match(/meetings\/([^/]+)\//) || [])[1];
            var body = join ? {{
                success: true,
                result: {{ user_id: 'u1', status: '{status}', is_host: false, joined_at: 0,
                           observer_token: 'obs-' + id }}
            }} : {{ success: true, result: {{}} }};
            var resp = new Response(JSON.stringify(body), {{
                status: 200, headers: {{ 'Content-Type': 'application/json' }}
            }});
            Object.defineProperty(resp, 'url', {{ value: req.url }});
            return new Promise(function(resolve) {{
                setTimeout(function() {{ resolve(resp); }}, join ? {delay_ms} : 0);
            }});
        }};
        window.WebSocket = function(url) {{
            var s = {{ url: url, readyState: 0, closed: false, binaryType: 'blob',
                       send: function() {{}},
                       close: function() {{ s.closed = true; s.readyState = 3; }},
                       addEventListener: function() {{}},
                       removeEventListener: function() {{}} }};
            window.__sockets.push(s);
            return s;
        }};
        window.WebSocket.CONNECTING = 0; window.WebSocket.OPEN = 1;
        window.WebSocket.CLOSING = 2; window.WebSocket.CLOSED = 3;
        "#
    ))
    .expect("failed to mock the network");
}

fn restore_network() {
    restore_fetch();
    js_sys::eval(
        "if (window.__original_ws) { window.WebSocket = window.__original_ws; delete window.__original_ws; }",
    )
    .unwrap();
}

fn fetches() -> Vec<String> {
    let v = js_sys::Reflect::get(&gloo_utils::window(), &"__fetches".into()).unwrap();
    js_sys::Array::from(&v)
        .iter()
        .filter_map(|x| x.as_string())
        .collect()
}

/// `(url, closed)` for every socket whose URL contains `needle`.
fn sockets(needle: &str) -> Vec<(String, bool)> {
    let v = js_sys::Reflect::get(&gloo_utils::window(), &"__sockets".into()).unwrap();
    js_sys::Array::from(&v)
        .iter()
        .filter_map(|s| {
            let url = js_sys::Reflect::get(&s, &"url".into()).ok()?.as_string()?;
            let closed = js_sys::Reflect::get(&s, &"closed".into()).ok()?.as_bool()?;
            url.contains(needle).then_some((url, closed))
        })
        .collect()
}

async fn wait_until(mut ok: impl FnMut() -> bool, timeout_ms: i32) -> bool {
    let mut waited = 0;
    while !ok() && waited < timeout_ms {
        sleep_ms(20).await;
        waited += 20;
    }
    ok()
}

async fn mount_meeting(join: Option<(&str, u32)>) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    if let Some((status, delay_ms)) = join {
        mock_network(status, delay_ms);
    }
    NAMED.with(|n| n.set(join.is_some()));
    let mount = create_mount_point();
    render_into(&mount, Harness);
    yield_now().await;
    mount
}

#[wasm_bindgen_test]
async fn a_new_meeting_id_rebuilds_the_meeting_tree() {
    let mount = mount_meeting(None).await;
    let before = name_input(&mount);

    click(&mount, "#jump");
    yield_now().await;
    let after = name_input(&mount);
    assert!(
        !before.is_same_node(Some(&after)),
        "a new meeting id must remount the meeting tree"
    );
    assert!(!before.is_connected(), "meeting A's tree must be torn down");
    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn a_jump_from_waiting_for_the_host_closes_meeting_as_lobby_socket() {
    let mount = mount_meeting(Some(("waiting_for_meeting", 0))).await;
    assert!(
        wait_until(|| !sockets("token=obs-meeting-a").is_empty(), 5_000).await,
        "meeting A's lobby socket should open; fetches {:?}",
        fetches()
    );

    click(&mount, "#jump");
    assert!(
        wait_until(|| sockets("token=obs-meeting-a").iter().all(|s| s.1), 2_000).await,
        "meeting A's lobby socket must close: {:?}",
        sockets("lobby")
    );
    cleanup(&mount);
    restore_network();
}

#[wasm_bindgen_test]
async fn a_jump_from_the_waiting_room_leaves_it_and_closes_its_socket() {
    let mount = mount_meeting(Some(("waiting", 0))).await;
    assert!(
        wait_until(|| !sockets("token=obs-meeting-a").is_empty(), 5_000).await,
        "the waiting room's lobby socket should open; fetches {:?}",
        fetches()
    );

    click(&mount, "#jump");
    let leave = |f: &[String]| {
        f.iter()
            .any(|f| f.starts_with("POST") && f.ends_with("/meetings/meeting-a/leave"))
    };
    assert!(
        wait_until(|| leave(&fetches()), 2_000).await,
        "leaving the waiting room must tell the server: {:?}",
        fetches()
    );
    assert!(
        sockets("token=obs-meeting-a").iter().all(|s| s.1),
        "the waiting room's lobby socket must close: {:?}",
        sockets("lobby")
    );
    cleanup(&mount);
    restore_network();
}

#[wasm_bindgen_test]
async fn a_join_answered_after_the_jump_does_not_touch_the_new_meeting() {
    let mount = mount_meeting(Some(("waiting_for_meeting", 300))).await;
    assert!(
        wait_until(
            || fetches().iter().any(|f| f.ends_with("/meeting-a/join")),
            2_000
        )
        .await,
        "meeting A's join should be in flight; fetches {:?}",
        fetches()
    );

    click(&mount, "#jump");
    sleep_ms(700).await;
    assert!(
        fetches().iter().any(|f| f.ends_with("/meeting-b/join")),
        "meeting B runs its own join: {:?}",
        fetches()
    );
    assert!(
        sockets("token=obs-meeting-a").is_empty(),
        "meeting A's late answer must not open a lobby socket: {:?}",
        sockets("lobby")
    );
    let errors = js_sys::eval("window.__errors.join('\\n')")
        .unwrap()
        .as_string();
    assert_eq!(
        errors.as_deref(),
        Some(""),
        "meeting A's late answer must not panic"
    );
    cleanup(&mount);
    restore_network();
}

thread_local! {
    static RECORDER_MOUNTED: std::cell::RefCell<Option<Signal<bool>>> =
        const { std::cell::RefCell::new(None) };
}

#[component]
fn Recorder() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    let state_cb: RecordingStateCallback = use_hook(Default::default);
    use_stop_recording_on_unmount(client, state_cb);
    rsx! {}
}

#[allow(non_snake_case)]
fn RecorderHarness() -> Element {
    let mounted = use_signal(|| true);
    use_hook(move || RECORDER_MOUNTED.with(|m| *m.borrow_mut() = Some(mounted)));
    rsx! {
        if mounted() {
            Recorder {}
        }
    }
}

fn fake_recorder(state: &str) {
    js_sys::eval(&format!(
        "window.__vcRecording = {{ stops: 0, getState: function() {{ return '{state}'; }}, \
         stop: function() {{ this.stops += 1; }} }};"
    ))
    .unwrap();
}

fn recorder_stops() -> f64 {
    js_sys::eval("window.__vcRecording.stops")
        .unwrap()
        .as_f64()
        .unwrap()
}

#[wasm_bindgen_test]
async fn unmounting_the_meeting_view_stops_a_running_recording_only() {
    for (state, stops) in [
        ("recording", 1.0),
        ("activating", 1.0),
        ("idle", 0.0),
        ("saving", 0.0),
    ] {
        fake_recorder(state);
        let mount = create_mount_point();
        render_into(&mount, RecorderHarness);
        yield_now().await;
        let mut mounted = RECORDER_MOUNTED.with(|m| m.borrow().expect("harness"));
        mounted.set(false);
        yield_now().await;
        assert_eq!(recorder_stops(), stops, "{state}");
        cleanup(&mount);
    }
    js_sys::eval("delete window.__vcRecording;").unwrap();
}

thread_local! {
    static ROOM: std::cell::RefCell<Option<Signal<bool>>> = const { std::cell::RefCell::new(None) };
    static GUEST: Cell<bool> = const { Cell::new(false) };
}

/// Answers every status poll with `status`; records `"METHOD url auth"`.
fn mock_status(status: &str) {
    js_sys::eval(&format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__fetches = [];
        window.fetch = function(input, init) {{
            var req = typeof input === 'string' ? new Request(input, init) : input;
            window.__fetches.push(req.method + ' ' + req.url + ' ' + req.headers.get('Authorization'));
            var body = /status$/.test(req.url) ? {{
                success: true,
                result: {{ user_id: 'w1', status: '{status}', is_host: false, joined_at: 0,
                           room_token: '{status}' === 'admitted' ? 'room-tok' : null }}
            }} : {{ success: true, result: {{}} }};
            var resp = new Response(JSON.stringify(body), {{
                status: 200, headers: {{ 'Content-Type': 'application/json' }}
            }});
            Object.defineProperty(resp, 'url', {{ value: req.url }});
            return Promise.resolve(resp);
        }};
        "#
    ))
    .expect("failed to mock the status API");
}

#[allow(non_snake_case)]
fn WaitingRoomHarness() -> Element {
    let transport = use_signal(|| TransportPreference::WebSocket);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let mut mounted = use_signal(|| true);
    use_hook(move || ROOM.with(|r| *r.borrow_mut() = Some(mounted)));
    rsx! {
        if mounted() {
            WaitingRoom {
                meeting_id: "room-a".to_string(),
                user_id: "w1".to_string(),
                display_name: "Waiter".to_string(),
                observer_token: "obs-held".to_string(),
                is_guest: GUEST.with(Cell::get),
                on_admitted: move |_| mounted.set(false),
                on_rejected: move |_| {},
                on_cancel: move |_| {},
                on_waiting_room_disabled: move |_| {},
            }
        }
    }
}

async fn mount_waiting_room(status: &str, guest: bool) -> web_sys::Element {
    reset_test_browser_state();
    inject_app_config();
    mock_status(status);
    GUEST.with(|g| g.set(guest));
    let mount = create_mount_point();
    render_into(&mount, WaitingRoomHarness);
    yield_now().await;
    mount
}

fn leaves() -> Vec<String> {
    fetches()
        .into_iter()
        .filter(|f| f.contains("/meetings/room-a/leave"))
        .collect()
}

#[wasm_bindgen_test]
async fn a_waiting_room_unmounted_after_admission_does_not_leave() {
    let mount = mount_waiting_room("admitted", false).await;
    assert!(
        wait_until(
            || ROOM.with(|r| !*r.borrow().expect("harness").peek()),
            2_000
        )
        .await,
        "the mount poll should admit; fetches {:?}",
        fetches()
    );
    sleep_ms(300).await;
    assert_eq!(leaves(), Vec::<String>::new());
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_guest_leaving_the_waiting_room_leaves_as_a_guest_with_its_token() {
    let mount = mount_waiting_room("waiting", true).await;
    sleep_ms(100).await;
    let mut mounted = ROOM.with(|r| r.borrow().expect("harness"));
    mounted.set(false);
    assert!(
        wait_until(|| !leaves().is_empty(), 2_000).await,
        "unmounting must leave; fetches {:?}",
        fetches()
    );
    assert_eq!(
        leaves(),
        ["POST http://test:8080/api/v1/meetings/room-a/leave-guest Bearer obs-held"]
    );
    cleanup(&mount);
    restore_fetch();
}
