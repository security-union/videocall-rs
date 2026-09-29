// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use dioxus_ui::components::presence_keepalive::PresenceKeepalive;
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
        window.__keepaliveFailFrom = {fail_from_js};
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
                var n = window.__keepaliveCount;
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
            return window.__original_fetch(input, init);
        }};
        "#
    );
    js_sys::eval(&script).expect("failed to mock the keepalive API");
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

async fn wait_for_count(at_least: u32, timeout_ms: u32) -> bool {
    let mut waited = 0u32;
    while keepalive_count() < at_least && waited < timeout_ms {
        sleep_ms(10).await;
        waited += 10;
    }
    keepalive_count() >= at_least
}

#[derive(Clone, Copy, Default)]
struct HarnessOpts {
    is_guest: bool,
}

thread_local! {
    static OPTS: RefCell<HarnessOpts> = RefCell::new(HarnessOpts::default());
}

#[allow(non_snake_case)]
fn Harness() -> Element {
    let joined = use_signal(|| false);
    let mounted = use_signal(|| true);
    use_hook(move || {
        JOINED.with(|j| *j.borrow_mut() = Some(joined));
        MOUNTED.with(|m| *m.borrow_mut() = Some(mounted));
    });
    let opts = OPTS.with(|o| *o.borrow());
    rsx! {
        if mounted() {
            PresenceKeepalive {
                meeting_id: MEETING_ID.to_string(),
                is_guest: opts.is_guest,
                observer_token: "observer-token".to_string(),
                meeting_joined: joined,
                interval_ms: Some(TEST_INTERVAL_MS),
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

fn mount_harness(is_guest: bool, fail_from: Option<u32>) -> web_sys::Element {
    // Must run before the mock is installed: it restores `window.fetch`.
    reset_test_browser_state();
    inject_app_config();
    mock_keepalive_api(fail_from);
    OPTS.with(|o| *o.borrow_mut() = HarnessOpts { is_guest });
    let mount = create_mount_point();
    render_into(&mount, Harness);
    mount
}

/// Waits for the effect to tear down its `Interval` before unmounting, so no
/// live `setInterval` survives into the next test in this binary.
async fn stop_and_cleanup(mount: &web_sys::Element) {
    set_joined(true);
    sleep_ms(300).await;
    cleanup(mount);
    restore_fetch();
}

#[wasm_bindgen_test]
async fn a_keepalive_fires_immediately_on_mount() {
    let mount = mount_harness(false, None);
    assert!(
        wait_for_count(1, 2_000).await,
        "one keepalive must fire immediately in the pre-join state, got {}",
        keepalive_count()
    );
    stop_and_cleanup(&mount).await;
}

#[wasm_bindgen_test]
async fn a_second_keepalive_fires_after_the_interval() {
    let mount = mount_harness(false, None);
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
    let mount = mount_harness(false, None);
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
    let mount = mount_harness(false, None);
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
    let mount = mount_harness(false, Some(2));
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
    let mount = mount_harness(true, None);
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
