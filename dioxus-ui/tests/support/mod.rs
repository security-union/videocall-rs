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
use futures::future::{AbortHandle, Abortable};
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

/// Owns a running Dioxus app and tears it down when dropped.
///
/// Dropping the handle aborts the future returned by [`dioxus::web::run`],
/// which drops the `VirtualDom` together with its scheduler, spawned tasks,
/// effects, and event-listener closures. Without this, every `render_into`
/// call would leave a "zombie" app that keeps waking on the scheduler for the
/// lifetime of the test binary — the runtime leak that made the wasm test
/// binaries accumulate work and intermittently blow past chromedriver's 300s
/// renderer timeout in CI.
#[must_use = "bind the AppHandle for the test's lifetime; dropping it tears down the Dioxus app"]
pub struct AppHandle {
    abort: AbortHandle,
}

impl Drop for AppHandle {
    fn drop(&mut self) {
        // `abort()` flags the shared abort state and wakes the spawned task.
        // On its next poll, `Abortable` short-circuits to `Err(Aborted)`,
        // completing (and thus dropping) the `run` future and everything it
        // owns. This is the only teardown path dioxus-web 0.7 exposes: the
        // public `launch`/`launch_virtual_dom` helpers are launch-and-forget
        // and return no handle, but `run` itself is a plain public async fn we
        // can own and cancel.
        self.abort.abort();
    }
}

/// Render a Dioxus component into the given mount element and return a handle
/// that tears the app down when dropped.
///
/// Wait one animation frame (via [`yield_now`]) after calling so the renderer
/// can flush its initial mutations. Bind the returned [`AppHandle`] for the
/// duration of the test — when it drops at end of scope the app runtime is
/// stopped, so the next test starts with zero live app runtimes regardless of
/// how many tests ran before it.
///
/// Use this in `#[wasm_bindgen_test] async fn` tests:
///
/// ```ignore
/// let mount = create_mount_point();
/// let _app = render_into(&mount, || rsx! { MyComponent { prop: "value" } });
/// yield_now().await;
/// // assert on mount.query_selector(...)
/// cleanup(&mount);
/// // `_app` drops here, aborting the app runtime.
/// ```
pub fn render_into(mount: &web_sys::Element, root: fn() -> Element) -> AppHandle {
    let cfg = dioxus::web::Config::new().rootelement(mount.clone());
    let vdom = VirtualDom::new(root);
    let (abort, registration) = AbortHandle::new_pair();
    // `dioxus::web::run` is the same infinite work-loop that
    // `launch_virtual_dom` spawns internally, but by owning the future we can
    // abort it. `run` returns `!`, so the async block's output type is `!`
    // (no trailing unit) and `Abortable`'s output is `Result<!, Aborted>`.
    let app = Abortable::new(
        async move { dioxus::web::run(vdom, cfg).await },
        registration,
    );
    wasm_bindgen_futures::spawn_local(async move {
        let _ = app.await;
    });
    AppHandle { abort }
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
    set("vadThreshold", &wasm_bindgen::JsValue::from(0.02));

    let frozen = js_sys::Object::freeze(&config);
    let window = gloo_utils::window();
    js_sys::Reflect::set(&window, &"__APP_CONFIG".into(), &frozen).unwrap();
}

/// Inject a `window.__APP_CONFIG` with a custom VAD threshold.
pub fn inject_app_config_with_vad_threshold(threshold: f32) {
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
    set("vadThreshold", &wasm_bindgen::JsValue::from(threshold));

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
