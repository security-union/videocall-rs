// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// wasm-bindgen integration tests for the merged MeetingsList component.
//
// Verifies the security-critical UI trust gating: owner-only affordances
// (inline gold star, edit, delete, tooltip "Owner" line) must render only
// when the server-provided `is_owner` flag is `true`. The backend regression
// test `test_two_identities_disjoint_is_owner_for_same_meeting` covers the
// wire boundary; these tests cover the UI binding.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, render_into, restore_fetch, yield_now};
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::meetings_list::MeetingsList;
use dioxus_ui::context::DisplayNameCtx;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Inject just the `meetingApiBaseUrl` field that `meeting_api_client()`
/// requires. The full `__APP_CONFIG` injection from `support` is heavier
/// than these tests need.
fn inject_minimal_config() {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("meetingApiBaseUrl", &"http://test:8081".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"false".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("firefoxEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("serverElectionPeriodMs", &wasm_bindgen::JsValue::from(2000));
    set("audioBitrateKbps", &wasm_bindgen::JsValue::from(65));
    set("videoBitrateKbps", &wasm_bindgen::JsValue::from(100));
    set("screenBitrateKbps", &wasm_bindgen::JsValue::from(100));
    set("vadThreshold", &wasm_bindgen::JsValue::from(0.02));
    let frozen = js_sys::Object::freeze(&config);
    js_sys::Reflect::set(&gloo_utils::window(), &"__APP_CONFIG".into(), &frozen).unwrap();
}

fn remove_config() {
    let _ = js_sys::Reflect::delete_property(&gloo_utils::window().into(), &"__APP_CONFIG".into());
}

/// Mock `fetch` so any request to `/api/v1/meetings/feed` returns the given
/// JSON body with status 200.
fn mock_fetch_feed(body: &str) {
    let escaped = body.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.fetch = function(input) {{
            var url = typeof input === 'string' ? input : input.url;
            var resp = new Response("{escaped}", {{
                status: 200,
                headers: {{ 'Content-Type': 'application/json' }}
            }});
            Object.defineProperty(resp, 'url', {{ value: url }});
            return Promise.resolve(resp);
        }};
        "#,
    );
    js_sys::eval(&script).expect("failed to mock fetch with feed body");
}

/// JSON body with one owned and one not-owned meeting (the wire shape the
/// backend's `list_feed` handler returns inside an `APIResponse` envelope).
fn feed_body_with_one_owned_one_not_owned() -> String {
    r#"{
        "success": true,
        "result": {
            "meetings": [
                {
                    "meeting_id": "owned-meeting-1",
                    "state": "active",
                    "last_active_at": 1714323600000,
                    "created_at": 1714323000000,
                    "started_at": 1714323500000,
                    "host": "alice@example.com",
                    "is_owner": true,
                    "participant_count": 2,
                    "waiting_count": 0,
                    "has_password": false,
                    "allow_guests": false,
                    "waiting_room_enabled": true,
                    "admitted_can_admit": false,
                    "end_on_host_leave": true
                },
                {
                    "meeting_id": "joined-meeting-1",
                    "state": "active",
                    "last_active_at": 1714323500000,
                    "created_at": 1714323000000,
                    "started_at": 1714323400000,
                    "host": "bob@example.com",
                    "is_owner": false,
                    "participant_count": 3,
                    "waiting_count": 0,
                    "has_password": false,
                    "allow_guests": false,
                    "waiting_room_enabled": true,
                    "admitted_can_admit": false,
                    "end_on_host_leave": true
                }
            ]
        }
    }"#
    .to_string()
}

/// Render `MeetingsList` inside the minimum context it needs. The component
/// reads `DisplayNameCtx` indirectly via login/auth components; providing a
/// neutral context keeps the harness honest.
fn meetings_list_wrapper() -> Element {
    let username_signal = use_signal(|| Some("Test".to_string()));
    use_context_provider(|| DisplayNameCtx(username_signal));
    rsx! {
        MeetingsList {}
    }
}

/// Wait for the mocked fetch to resolve and Dioxus to re-render. We yield
/// once for the initial render, then sleep ~150ms (enough for two
/// microtask turns plus the simulated fetch round-trip) and yield again.
async fn wait_for_fetch_to_render() {
    yield_now().await;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 150)
            .unwrap();
    });
    wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    yield_now().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn meetings_list_renders_owner_only_affordances_for_owned_row() {
    inject_minimal_config();
    mock_fetch_feed(&feed_body_with_one_owned_one_not_owned());

    let mount = create_mount_point();
    render_into(&mount, meetings_list_wrapper);
    wait_for_fetch_to_render().await;

    let items = mount.query_selector_all(".meeting-item").unwrap();
    assert_eq!(
        items.length(),
        2,
        "expected exactly two meeting rows; got {}",
        items.length()
    );

    // Star icons render only for is_owner=true rows.
    let stars = mount.query_selector_all(".meeting-owner-icon").unwrap();
    assert_eq!(
        stars.length(),
        1,
        "expected exactly one inline star icon (owned row only); got {}",
        stars.length()
    );

    // Edit + delete buttons render only for is_owner=true rows.
    let edits = mount.query_selector_all(".meeting-edit-btn").unwrap();
    assert_eq!(
        edits.length(),
        1,
        "expected exactly one edit button (owned row only); got {}",
        edits.length()
    );
    let deletes = mount.query_selector_all(".meeting-delete-btn").unwrap();
    assert_eq!(
        deletes.length(),
        1,
        "expected exactly one delete button (owned row only); got {}",
        deletes.length()
    );

    // Defensive: the pre-existing pill class must never come back.
    let badges = mount.query_selector_all(".meeting-owner-badge").unwrap();
    assert_eq!(
        badges.length(),
        0,
        "stale .meeting-owner-badge pill must not be re-introduced; found {}",
        badges.length()
    );

    cleanup(&mount);
    restore_fetch();
    remove_config();
}

#[wasm_bindgen_test]
async fn meetings_list_omits_owner_affordances_for_not_owned_row() {
    inject_minimal_config();
    // Single not-owned meeting — easier to assert an empty trust UI.
    let body = r#"{
        "success": true,
        "result": {
            "meetings": [
                {
                    "meeting_id": "joined-only-1",
                    "state": "active",
                    "last_active_at": 1714323500000,
                    "created_at": 1714323000000,
                    "started_at": 1714323400000,
                    "host": "bob@example.com",
                    "is_owner": false,
                    "participant_count": 3,
                    "waiting_count": 0,
                    "has_password": false,
                    "allow_guests": false,
                    "waiting_room_enabled": true,
                    "admitted_can_admit": false,
                    "end_on_host_leave": true
                }
            ]
        }
    }"#;
    mock_fetch_feed(body);

    let mount = create_mount_point();
    render_into(&mount, meetings_list_wrapper);
    wait_for_fetch_to_render().await;

    let items = mount.query_selector_all(".meeting-item").unwrap();
    assert_eq!(items.length(), 1, "expected exactly one row");

    let stars = mount.query_selector_all(".meeting-owner-icon").unwrap();
    assert_eq!(
        stars.length(),
        0,
        "non-owner row must not render the inline star icon; got {}",
        stars.length()
    );

    let edits = mount.query_selector_all(".meeting-edit-btn").unwrap();
    assert_eq!(
        edits.length(),
        0,
        "non-owner row must not render the edit button; got {}",
        edits.length()
    );
    let deletes = mount.query_selector_all(".meeting-delete-btn").unwrap();
    assert_eq!(
        deletes.length(),
        0,
        "non-owner row must not render the delete button; got {}",
        deletes.length()
    );

    cleanup(&mount);
    restore_fetch();
    remove_config();
}
