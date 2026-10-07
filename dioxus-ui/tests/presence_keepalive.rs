// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use dioxus_ui::components::presence_keepalive::PresenceKeepalive;
use dioxus_ui::components::waiting_room::WaitingRoom;
use dioxus_ui::context::{TransportPreference, TransportPreferenceCtx};
use support::{
    cleanup, create_mount_point, inject_app_config, render_into, reset_test_browser_state,
    restore_fetch,
};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const MEETING_ID: &str = "m1";
const TEST_INTERVAL_MS: u32 = 150;

thread_local! {
    static JOINED: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static MOUNTED: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static TOKEN: RefCell<Option<Signal<String>>> = const { RefCell::new(None) };
    static REJOINS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

/// `fail_from`: the 1-indexed call number (and every call after) that
/// returns 404 `PARTICIPANT_NOT_FOUND`; `None` never fails.
fn mock_keepalive_api(fail_from: Option<u32>) {
    let fail_from_js = fail_from
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string());
    let script = format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.__keepaliveCount = 0;
        window.__keepaliveLastUrl = null;
        window.__keepaliveAuths = [];
        window.__keepaliveHangOn = null;
        window.__keepaliveFailFrom = {fail_from_js};
        window.__statusUrls = [];
        window.__statusToken = null;
        window.__statusWr = true;
        window.fetch = function(input, init) {{
            var req = typeof input === 'string' ? null : input;
            var url = req ? req.url : input;
            var respond = function(status, body) {{
                var resp = new Response(JSON.stringify(body), {{
                    status: status, headers: {{ 'Content-Type': 'application/json' }}
                }});
                // reqwest requires a real `Response.url`; a constructed one defaults to "".
                Object.defineProperty(resp, 'url', {{ value: url }});
                return resp;
            }};
            if (url.indexOf('/presence/keepalive') !== -1) {{
                window.__keepaliveCount += 1;
                window.__keepaliveLastUrl = url;
                window.__keepaliveAuths.push(req ? req.headers.get('Authorization') : null);
                var n = window.__keepaliveCount;
                if (window.__keepaliveHangOn === n) {{
                    return new Promise(function() {{}});
                }}
                var failFrom = window.__keepaliveFailFrom;
                if (failFrom !== null && n >= failFrom) {{
                    return Promise.resolve(respond(404, {{
                        success: false, result: {{
                            code: 'PARTICIPANT_NOT_FOUND', message: 'gone'
                        }}
                    }}));
                }}
                return Promise.resolve(respond(200, {{ success: true, result: null }}));
            }}
            if (/\/(guest-)?status$/.test(url)) {{
                window.__statusUrls.push(url);
                var row = {{
                    user_id: 'w1', display_name: 'Waiter', status: 'waiting',
                    is_host: false, joined_at: 0,
                    waiting_room_enabled: window.__statusWr !== false
                }};
                if (window.__statusToken) {{ row.observer_token = window.__statusToken; }}
                return Promise.resolve(respond(200, {{ success: true, result: row }}));
            }}
            return window.__original_fetch(input, init);
        }};
        "#
    );
    js_sys::eval(&script).expect("failed to mock the keepalive API");
}

fn js(script: &str) {
    js_sys::eval(script).expect("test script failed");
}

fn keepalive_count() -> u32 {
    js_sys::Reflect::get(&gloo_utils::window(), &"__keepaliveCount".into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

fn keepalive_last_url() -> Option<String> {
    js_sys::Reflect::get(&gloo_utils::window(), &"__keepaliveLastUrl".into())
        .ok()
        .and_then(|v| v.as_string())
}

fn js_strings(name: &str) -> Vec<Option<String>> {
    let value = js_sys::Reflect::get(&gloo_utils::window(), &name.into()).unwrap();
    js_sys::Array::from(&value)
        .iter()
        .map(|v| v.as_string())
        .collect()
}

fn keepalive_auths() -> Vec<Option<String>> {
    js_strings("__keepaliveAuths")
}

async fn wait_for_count(at_least: u32, timeout_ms: u32) -> bool {
    let mut waited = 0u32;
    while keepalive_count() < at_least && waited < timeout_ms {
        sleep_ms(10).await;
        waited += 10;
    }
    keepalive_count() >= at_least
}

#[derive(Clone, Copy)]
struct HarnessOpts {
    is_guest: bool,
    interval_ms: u32,
    stop_on_not_found: bool,
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            is_guest: false,
            interval_ms: TEST_INTERVAL_MS,
            stop_on_not_found: true,
        }
    }
}

thread_local! {
    static OPTS: RefCell<HarnessOpts> = RefCell::new(HarnessOpts::default());
}

#[allow(non_snake_case)]
fn Harness() -> Element {
    let joined = use_signal(|| false);
    let mounted = use_signal(|| true);
    let token = use_signal(|| "observer-token".to_string());
    use_hook(move || {
        JOINED.with(|j| *j.borrow_mut() = Some(joined));
        MOUNTED.with(|m| *m.borrow_mut() = Some(mounted));
        TOKEN.with(|t| *t.borrow_mut() = Some(token));
    });
    let opts = OPTS.with(|o| *o.borrow());
    rsx! {
        if mounted() {
            PresenceKeepalive {
                meeting_id: MEETING_ID.to_string(),
                is_guest: opts.is_guest,
                observer_token: token(),
                meeting_joined: joined,
                stop_on_not_found: opts.stop_on_not_found,
                interval_ms: Some(opts.interval_ms),
            }
        }
    }
}

#[allow(non_snake_case)]
fn WaitingRoomHarness() -> Element {
    let transport = use_signal(TransportPreference::default);
    use_context_provider(|| TransportPreferenceCtx(transport));
    let mounted = use_signal(|| true);
    use_hook(move || MOUNTED.with(|m| *m.borrow_mut() = Some(mounted)));
    let opts = OPTS.with(|o| *o.borrow());
    rsx! {
        if mounted() {
            WaitingRoom {
                meeting_id: MEETING_ID.to_string(),
                user_id: "w1".to_string(),
                display_name: "Waiter".to_string(),
                // Empty keeps the observer socket out of the test; the token
                // the keepalive sends then comes only from a status poll.
                observer_token: String::new(),
                is_guest: opts.is_guest,
                on_admitted: move |_| {},
                on_rejected: move |_| {},
                on_cancel: move |_| {},
                on_waiting_room_disabled: move |_| REJOINS.with(|r| r.set(r.get() + 1)),
                keepalive_interval_ms: Some(opts.interval_ms),
            }
        }
    }
}

fn set_joined(v: bool) {
    let mut s = JOINED.with(|j| j.borrow().expect("joined signal"));
    s.set(v);
}

fn set_mounted(v: bool) {
    let mut s = MOUNTED.with(|m| m.borrow().expect("mounted signal"));
    s.set(v);
}

fn set_token(v: &str) {
    let mut s = TOKEN.with(|t| t.borrow().expect("token signal"));
    s.set(v.to_string());
}

/// Unmounts whatever a test that panicked before its teardown left running.
async fn unmount_leftovers() {
    let Some(mut mounted) = MOUNTED.with(|m| m.borrow_mut().take()) else {
        return;
    };
    if mounted.try_peek().map(|v| *v).unwrap_or(false) {
        mounted.set(false);
        sleep_ms(300).await;
    }
}

/// `setup` runs against the installed mock, before the first render.
async fn mount_with(
    opts: HarnessOpts,
    fail_from: Option<u32>,
    setup: &str,
    root: fn() -> Element,
) -> web_sys::Element {
    unmount_leftovers().await;
    // Must run before the mock is installed: it restores `window.fetch`.
    reset_test_browser_state();
    inject_app_config();
    mock_keepalive_api(fail_from);
    js(setup);
    OPTS.with(|o| *o.borrow_mut() = opts);
    REJOINS.with(|r| r.set(0));
    let mount = create_mount_point();
    render_into(&mount, root);
    mount
}

async fn mount_harness(is_guest: bool, fail_from: Option<u32>) -> web_sys::Element {
    mount_with(
        HarnessOpts {
            is_guest,
            ..HarnessOpts::default()
        },
        fail_from,
        "",
        Harness,
    )
    .await
}

/// Waits for the effect to tear down its `Interval` before unmounting, so no
/// live `setInterval` survives into the next test in this binary.
async fn stop_and_cleanup(mount: &web_sys::Element) {
    set_joined(true);
    sleep_ms(300).await;
    cleanup(mount);
    restore_fetch();
}

async fn unmount_and_cleanup(mount: &web_sys::Element) {
    set_mounted(false);
    sleep_ms(300).await;
    cleanup(mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_keepalive_fires_immediately_on_mount() {
    let mount = mount_harness(false, None).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "one keepalive must fire immediately in the pre-join state, got {}",
        keepalive_count()
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_second_keepalive_fires_after_the_interval() {
    let mount = mount_harness(false, None).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land first"
    );
    assert!(
        wait_for_count(2, TEST_INTERVAL_MS * 4).await,
        "a second keepalive must fire after the interval, got {}",
        keepalive_count()
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn no_keepalive_fires_after_join() {
    let mount = mount_harness(false, None).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land first"
    );
    set_joined(true);
    sleep_ms(300).await;
    let count_at_join = keepalive_count();
    sleep_ms((TEST_INTERVAL_MS * 5) as i32).await;
    assert_eq!(
        keepalive_count(),
        count_at_join,
        "no keepalive may fire once meeting_joined() is true"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn no_keepalive_fires_after_unmount() {
    let mount = mount_harness(false, None).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land first"
    );
    set_mounted(false);
    sleep_ms(300).await;
    let count_at_unmount = keepalive_count();
    sleep_ms((TEST_INTERVAL_MS * 5) as i32).await;
    assert_eq!(
        keepalive_count(),
        count_at_unmount,
        "no keepalive may fire once the component has unmounted"
    );
    cleanup(&mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_404_stops_further_keepalives_even_though_not_joined() {
    let mount = mount_harness(false, Some(2)).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land first"
    );
    assert!(
        wait_for_count(2, TEST_INTERVAL_MS * 4).await,
        "the second (404) call must still land"
    );
    sleep_ms(300).await;
    let count_after_404 = keepalive_count();
    sleep_ms((TEST_INTERVAL_MS * 5) as i32).await;
    assert_eq!(
        keepalive_count(),
        count_after_404,
        "a 404 (PARTICIPANT_NOT_FOUND) must stop the loop, not just this one call"
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_guest_calls_the_guest_endpoint() {
    let mount = mount_harness(true, None).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land"
    );
    let url = keepalive_last_url().unwrap_or_default();
    assert!(
        url.ends_with("/presence/keepalive-guest"),
        "a guest must call the guest endpoint, got {url}"
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_hung_keepalive_does_not_block_later_renewals() {
    let mount = mount_with(
        HarnessOpts::default(),
        None,
        "window.__keepaliveHangOn = 1;",
        Harness,
    )
    .await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land (and never answer)"
    );
    assert!(
        wait_for_count(2, TEST_INTERVAL_MS * 4).await,
        "a keepalive that never answers must not hold every later tick, got {} call(s)",
        keepalive_count()
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn with_stop_on_not_found_off_a_404_keeps_renewing() {
    let mount = mount_with(
        HarnessOpts {
            stop_on_not_found: false,
            ..HarnessOpts::default()
        },
        Some(1),
        "",
        Harness,
    )
    .await;
    assert!(
        wait_for_count(3, TEST_INTERVAL_MS * 6).await,
        "a 404 must not stop the loop when stop_on_not_found is false, got {} call(s)",
        keepalive_count()
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_new_observer_token_goes_out_on_the_next_tick_without_an_extra_call() {
    const INTERVAL_MS: u32 = 400;
    let mount = mount_with(
        HarnessOpts {
            is_guest: true,
            interval_ms: INTERVAL_MS,
            ..HarnessOpts::default()
        },
        None,
        "",
        Harness,
    )
    .await;
    assert!(wait_for_count(2, INTERVAL_MS * 4).await, "a tick must land");
    set_token("fresh-token");
    sleep_ms((INTERVAL_MS / 4) as i32).await;
    assert_eq!(
        keepalive_count(),
        2,
        "a new token must not restart the interval or fire an extra keepalive"
    );
    assert!(
        wait_for_count(3, INTERVAL_MS * 3).await,
        "the next tick must land"
    );
    let auths = keepalive_auths();
    assert_eq!(
        auths[..3],
        [
            Some("Bearer observer-token".to_string()),
            Some("Bearer observer-token".to_string()),
            Some("Bearer fresh-token".to_string()),
        ],
        "the tick after a token change must carry the new token"
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn the_waiting_room_renews_a_signed_in_waiter_through_the_cookie_endpoint() {
    let mount = mount_with(HarnessOpts::default(), None, "", WaitingRoomHarness).await;
    assert!(
        wait_for_count(2, 2_000).await,
        "a signed-in waiter must keep renewing, got {} call(s)",
        keepalive_count()
    );
    let url = keepalive_last_url().unwrap_or_default();
    assert!(
        url.ends_with("/presence/keepalive"),
        "a signed-in waiter renews through the cookie endpoint, got {url}"
    );
    assert!(
        keepalive_auths().iter().all(Option::is_none),
        "the cookie endpoint carries no bearer token"
    );
    unmount_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn the_waiting_room_renews_a_guest_with_the_token_its_status_poll_returned() {
    let mount = mount_with(
        HarnessOpts {
            is_guest: true,
            ..HarnessOpts::default()
        },
        None,
        "window.__statusToken = 'polled-token';",
        WaitingRoomHarness,
    )
    .await;
    let mut waited = 0;
    while !keepalive_auths().contains(&Some("Bearer polled-token".to_string())) && waited < 2_000 {
        sleep_ms(10).await;
        waited += 10;
    }
    assert!(
        keepalive_auths().contains(&Some("Bearer polled-token".to_string())),
        "the guest keepalive must send the token from the latest status poll, sent {:?}",
        keepalive_auths()
    );
    let url = keepalive_last_url().unwrap_or_default();
    assert!(
        url.ends_with("/presence/keepalive-guest"),
        "a guest waiter renews through the guest endpoint, got {url}"
    );
    assert!(
        js_strings("__statusUrls")
            .iter()
            .flatten()
            .all(|u| u.ends_with("/guest-status")),
        "a guest waiter polls guest-status"
    );
    unmount_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn the_waiting_room_keeps_renewing_through_a_404() {
    let mount = mount_with(HarnessOpts::default(), Some(1), "", WaitingRoomHarness).await;
    assert!(
        wait_for_count(3, TEST_INTERVAL_MS * 6).await,
        "a waiter still on the page keeps renewing after a 404 (e.g. while the meeting \
         is ended and may restart), got {} call(s)",
        keepalive_count()
    );
    unmount_and_cleanup(&mount).await;
}

fn rejoins() -> u32 {
    REJOINS.with(|r| r.get())
}

async fn wait_for_rejoin(timeout_ms: u32) -> bool {
    let mut waited = 0;
    while rejoins() == 0 && waited < timeout_ms {
        sleep_ms(20).await;
        waited += 20;
    }
    rejoins() > 0
}

#[wasm_bindgen_test]
async fn a_waiter_still_waiting_after_the_waiting_room_turned_off_re_joins_once() {
    let mount = mount_with(
        HarnessOpts::default(),
        None,
        "window.__statusWr = false;",
        WaitingRoomHarness,
    )
    .await;
    assert!(
        wait_for_rejoin(2_000).await,
        "the mount poll must ask to re-join"
    );
    sleep_ms(6_000).await;
    assert_eq!(
        rejoins(),
        1,
        "later polls must not ask again while the re-join is under way"
    );
    unmount_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_waiter_re_joins_when_a_later_poll_finds_the_waiting_room_off() {
    let mount = mount_with(HarnessOpts::default(), None, "", WaitingRoomHarness).await;
    assert!(wait_for_count(1, 2_000).await, "the waiting room is up");
    sleep_ms(300).await;
    assert_eq!(rejoins(), 0, "the waiting room is still on");
    js("window.__statusWr = false;");
    assert!(
        wait_for_rejoin(7_000).await,
        "the next status poll must ask to re-join"
    );
    unmount_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn no_waiting_room_keepalive_fires_after_the_waiting_room_unmounts() {
    let mount = mount_with(HarnessOpts::default(), None, "", WaitingRoomHarness).await;
    assert!(
        wait_for_count(1, 2_000).await,
        "the immediate call must land"
    );
    set_mounted(false);
    sleep_ms(300).await;
    let count_at_unmount = keepalive_count();
    sleep_ms((TEST_INTERVAL_MS * 5) as i32).await;
    assert_eq!(
        keepalive_count(),
        count_at_unmount,
        "admission, rejection or cancel unmounts the waiting room; no renewal may follow"
    );
    cleanup(&mount);
    restore_fetch();
}
