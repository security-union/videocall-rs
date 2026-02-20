// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Shared test harness for dioxus-ui component tests.
//
// Provides mount/cleanup helpers, mock device construction, and
// Dioxus rendering helpers so that individual test files stay
// focused on assertions rather than boilerplate.
#![allow(dead_code)]

use dioxus::prelude::*;
use js_sys::Array;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{MediaDeviceInfo, MediaDeviceKind, MediaStreamConstraints};

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

/// Create a fresh `<div>`, attach it to `<body>`, and return it.
pub fn create_mount_point() -> web_sys::Element {
    let document = gloo_utils::document();
    let div = document.create_element("div").unwrap();
    document.body().unwrap().append_child(&div).unwrap();
    div
}

/// Remove the mount-point from `<body>` so subsequent tests start clean.
pub fn cleanup(mount: &web_sys::Element) {
    gloo_utils::document()
        .body()
        .unwrap()
        .remove_child(mount)
        .ok();
}

// ---------------------------------------------------------------------------
// Dioxus rendering helper
// ---------------------------------------------------------------------------

/// Render a Dioxus component into the given mount element and wait one
/// animation frame for the renderer to flush its initial mutations.
///
/// Use this in `#[wasm_bindgen_test] async fn` tests:
///
/// ```ignore
/// let mount = create_mount_point();
/// render_into(&mount, || rsx! { MyComponent { prop: "value" } });
/// yield_now().await;
/// // assert on mount.query_selector(...)
/// cleanup(&mount);
/// ```
pub fn render_into(mount: &web_sys::Element, root: fn() -> Element) {
    let cfg = dioxus::web::Config::new().rootelement(mount.clone());
    dioxus::web::launch::launch_virtual_dom(VirtualDom::new(root), cfg);
}

/// Yield to the browser event loop so Dioxus can process its initial render.
///
/// Similar to `yew::platform::time::sleep(Duration::ZERO)` but using a
/// `requestAnimationFrame`-based promise.
pub async fn yield_now() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        // requestAnimationFrame fires after the current microtask queue is drained
        // and before the next paint, giving Dioxus time to apply its mutations.
        gloo_utils::window()
            .request_animation_frame(&resolve)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
    // Second yield to ensure mutations are flushed
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        gloo_utils::window()
            .request_animation_frame(&resolve)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

// ---------------------------------------------------------------------------
// Runtime config injection (integration tests)
// ---------------------------------------------------------------------------

/// Inject a `window.__APP_CONFIG` object with OAuth disabled and all
/// required `RuntimeConfig` fields.
pub fn inject_app_config() {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
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

    let frozen = js_sys::Object::freeze(&config);
    let window = gloo_utils::window();
    js_sys::Reflect::set(&window, &"__APP_CONFIG".into(), &frozen).unwrap();
}

/// Remove `window.__APP_CONFIG` so tests don't leak state.
pub fn remove_app_config() {
    let window = gloo_utils::window();
    let _ = js_sys::Reflect::delete_property(&window.into(), &"__APP_CONFIG".into());
}

// ---------------------------------------------------------------------------
// Mock device construction
// ---------------------------------------------------------------------------

/// Build a `MediaDeviceInfo`-compatible JS object with the given properties.
pub fn create_mock_device(id: &str, kind: &str, label: &str) -> MediaDeviceInfo {
    let device = js_sys::Object::new();
    js_sys::Reflect::set(&device, &"deviceId".into(), &id.into()).unwrap();
    js_sys::Reflect::set(&device, &"kind".into(), &kind.into()).unwrap();
    js_sys::Reflect::set(&device, &"label".into(), &label.into()).unwrap();
    js_sys::Reflect::set(&device, &"groupId".into(), &"test-group".into()).unwrap();
    device.unchecked_into::<MediaDeviceInfo>()
}

pub fn mock_mic(id: &str, label: &str) -> MediaDeviceInfo {
    create_mock_device(id, "audioinput", label)
}

pub fn mock_camera(id: &str, label: &str) -> MediaDeviceInfo {
    create_mock_device(id, "videoinput", label)
}

pub fn mock_speaker(id: &str, label: &str) -> MediaDeviceInfo {
    create_mock_device(id, "audiooutput", label)
}

// ---------------------------------------------------------------------------
// Fetch mocking
// ---------------------------------------------------------------------------

pub fn mock_fetch_401() {
    js_sys::eval(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.fetch = function(input) {
            var url = typeof input === 'string' ? input : input.url;
            var resp = new Response('{"error":"Unauthorized"}', {
                status: 401,
                headers: { 'Content-Type': 'application/json' }
            });
            Object.defineProperty(resp, 'url', { value: url });
            return Promise.resolve(resp);
        };
        "#,
    )
    .expect("failed to mock fetch with 401");
}

pub fn mock_fetch_meetings_empty() {
    js_sys::eval(
        r#"
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.fetch = function(input) {
            var url = typeof input === 'string' ? input : input.url;
            var resp = new Response(JSON.stringify({
                success: true,
                result: { meetings: [], total: 0, limit: 20, offset: 0 }
            }), {
                status: 200,
                headers: { 'Content-Type': 'application/json' }
            });
            Object.defineProperty(resp, 'url', { value: url });
            return Promise.resolve(resp);
        };
        "#,
    )
    .expect("failed to mock fetch with empty meetings");
}

pub fn restore_fetch() {
    js_sys::eval(
        r#"
        if (window.__original_fetch) {
            window.fetch = window.__original_fetch;
            delete window.__original_fetch;
        }
        "#,
    )
    .expect("failed to restore fetch");
}

// ---------------------------------------------------------------------------
// Real Chrome fake-device enumeration
// ---------------------------------------------------------------------------

pub async fn enumerate_fake_devices() -> (
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
) {
    let navigator = gloo_utils::window().navigator();
    let media_devices = navigator.media_devices().expect("media_devices API");

    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&true.into());
    constraints.set_video(&true.into());
    let stream_promise = media_devices
        .get_user_media_with_constraints(&constraints)
        .expect("getUserMedia");
    let _stream: web_sys::MediaStream = JsFuture::from(stream_promise)
        .await
        .expect("getUserMedia resolved")
        .unchecked_into();

    let enum_promise = media_devices.enumerate_devices().expect("enumerateDevices");
    let devices_js = JsFuture::from(enum_promise)
        .await
        .expect("enumerateDevices resolved");
    let array: Array = devices_js.unchecked_into();

    let all: Vec<MediaDeviceInfo> = array
        .iter()
        .map(|v| v.unchecked_into::<MediaDeviceInfo>())
        .collect();

    let mics: Vec<MediaDeviceInfo> = all
        .iter()
        .filter(|d| d.kind() == MediaDeviceKind::Audioinput)
        .cloned()
        .collect();
    let cams: Vec<MediaDeviceInfo> = all
        .iter()
        .filter(|d| d.kind() == MediaDeviceKind::Videoinput)
        .cloned()
        .collect();
    let speakers: Vec<MediaDeviceInfo> = all
        .iter()
        .filter(|d| d.kind() == MediaDeviceKind::Audiooutput)
        .cloned()
        .collect();

    (mics, cams, speakers)
}
