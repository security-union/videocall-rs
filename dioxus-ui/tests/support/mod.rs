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
use futures::future::{select, Either};
use gloo_timers::future::TimeoutFuture;
use js_sys::Array;
use std::pin::pin;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
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
// Timeout utility for headless Chrome tests
// ---------------------------------------------------------------------------

async fn await_js_promise_with_timeout(
    promise: js_sys::Promise,
    operation: &str,
    timeout_ms: u32,
) -> Result<JsValue, String> {
    let promise_fut = pin!(JsFuture::from(promise));
    let timeout_fut = pin!(TimeoutFuture::new(timeout_ms));

    match select(promise_fut, timeout_fut).await {
        Either::Left((Ok(value), _)) => Ok(value),
        Either::Left((Err(e), _)) => Err(format!("{} failed: {:?}", operation, e)),
        Either::Right((_, _)) => Err(format!("{} timed out after {}ms", operation, timeout_ms)),
    }
}

/// Wrap getUserMedia with a real fail-fast timeout.
///
/// Races the browser promise against a timer future so we never block forever
/// if headless Chrome hangs on a permission dialog.
pub async fn get_user_media_with_timeout(
    media_devices: &web_sys::MediaDevices,
    timeout_ms: u32,
) -> Result<web_sys::MediaStream, String> {
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&true.into());
    constraints.set_video(&true.into());

    let promise = media_devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| format!("getUserMedia setup failed: {:?}", e))?;

    await_js_promise_with_timeout(promise, "getUserMedia", timeout_ms)
        .await
        .map(|stream_js| stream_js.unchecked_into())
}

pub async fn enumerate_devices_with_timeout(
    media_devices: &web_sys::MediaDevices,
    timeout_ms: u32,
) -> Result<Vec<MediaDeviceInfo>, String> {
    let promise = media_devices
        .enumerate_devices()
        .map_err(|e| format!("enumerateDevices setup failed: {:?}", e))?;

    let devices_js = await_js_promise_with_timeout(promise, "enumerateDevices", timeout_ms).await?;
    let array: Array = devices_js.unchecked_into();
    Ok(array
        .iter()
        .map(|v| v.unchecked_into::<MediaDeviceInfo>())
        .collect())
}

// ---------------------------------------------------------------------------
// Real Chrome fake-device enumeration with fallback
// ---------------------------------------------------------------------------

/// Enumerate real fake devices from Chrome, with fallback to mock devices if timeouts occur.
///
/// In headless Chrome, getUserMedia can hang if permissions aren't properly configured.
/// This function:
/// 1. Tries to call getUserMedia and enumerateDevices with a 5-second timeout
/// 2. Falls back to synthetic mock devices if enumeration fails
/// 3. Logs warnings to help diagnose CI issues
pub async fn enumerate_fake_devices() -> (
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
) {
    let navigator = gloo_utils::window().navigator();
    let media_devices = match navigator.media_devices() {
        Ok(md) => md,
        Err(_) => {
            web_sys::console::warn_1(&"MediaDevices API unavailable, using fallback mocks".into());
            return get_fallback_fake_devices();
        }
    };

    const TIMEOUT_MS: u32 = 5000; // 5-second timeout

    // Step 1: Try to call getUserMedia to trigger permission grants
    web_sys::console::log_1(&"Calling getUserMedia with 5s timeout...".into());
    match get_user_media_with_timeout(&media_devices, TIMEOUT_MS).await {
        Ok(_stream) => {
            web_sys::console::log_1(&"getUserMedia succeeded, enumerating devices...".into());
        }
        Err(e) => {
            web_sys::console::warn_1(
                &format!("getUserMedia failed (will fallback to mocks): {}", e).into(),
            );
            return get_fallback_fake_devices();
        }
    }

    // Step 2: Enumerate devices with timeout
    web_sys::console::log_1(&"Calling enumerateDevices with 5s timeout...".into());
    match enumerate_devices_with_timeout(&media_devices, TIMEOUT_MS).await {
        Ok(all_devices) => {
            let mics: Vec<MediaDeviceInfo> = all_devices
                .iter()
                .filter(|d| d.kind() == MediaDeviceKind::Audioinput)
                .cloned()
                .collect();
            let cams: Vec<MediaDeviceInfo> = all_devices
                .iter()
                .filter(|d| d.kind() == MediaDeviceKind::Videoinput)
                .cloned()
                .collect();
            let speakers: Vec<MediaDeviceInfo> = all_devices
                .iter()
                .filter(|d| d.kind() == MediaDeviceKind::Audiooutput)
                .cloned()
                .collect();

            web_sys::console::log_1(
                &format!(
                    "Enumeration succeeded: {} mics, {} cameras, {} speakers",
                    mics.len(),
                    cams.len(),
                    speakers.len()
                )
                .into(),
            );
            (mics, cams, speakers)
        }
        Err(e) => {
            web_sys::console::warn_1(
                &format!("enumerateDevices failed (using fallback mocks): {}", e).into(),
            );
            get_fallback_fake_devices()
        }
    }
}

/// Fallback synthetic devices for headless Chrome when real enumeration times out.
fn get_fallback_fake_devices() -> (
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
) {
    web_sys::console::warn_1(
        &"Using synthetic mock devices (headless Chrome or permission issue)".into(),
    );

    let mics = vec![
        mock_mic("headset-default-audio-input", "Headset (Fake)"),
        mock_mic("internal-mic-1", "Internal Microphone (Fake)"),
    ];

    let cams = vec![
        mock_camera("headset-default-video-input", "Headset Camera (Fake)"),
        mock_camera("internal-camera-1", "Internal Camera (Fake)"),
    ];

    let speakers = vec![
        mock_speaker("headset-default-audio-output", "Headset Speaker (Fake)"),
        mock_speaker("internal-speaker-1", "Internal Speaker (Fake)"),
    ];

    (mics, cams, speakers)
}
