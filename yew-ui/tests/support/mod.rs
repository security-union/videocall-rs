// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Shared test harness for yew-ui component tests.
//
// Provides mount/cleanup helpers, mock device construction, and
// real Chrome fake-device enumeration so that individual test files
// stay focused on assertions rather than boilerplate.
//
// Each test file that does `mod support;` compiles its own copy, so not every
// function is used in every compilation unit.
#![allow(dead_code)]

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
// Runtime config injection (integration tests)
// ---------------------------------------------------------------------------

/// Inject a `window.__APP_CONFIG` object with OAuth disabled and all
/// required `RuntimeConfig` fields.  Call this before rendering any
/// component that reads the runtime config (e.g. `Home`, `AppRoot`).
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

/// Inject a `window.__APP_CONFIG` with OAuth enabled and a specific provider.
/// `provider` should be `"google"`, `"okta"`, or `""` for generic.
pub fn inject_app_config_with_provider(provider: &str) {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("meetingApiBaseUrl", &"http://test:8081".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"true".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("firefoxEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("oauthProvider", &provider.into());
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
// Mock device construction (Layer 2 tests)
// ---------------------------------------------------------------------------

/// Build a `MediaDeviceInfo`-compatible JS object with the given properties.
///
/// `web_sys::MediaDeviceInfo` accessors use *structural* getters
/// (`Reflect::get`), so plain properties on a `js_sys::Object` work
/// correctly — no prototype tricks needed.
pub fn create_mock_device(id: &str, kind: &str, label: &str) -> MediaDeviceInfo {
    let device = js_sys::Object::new();
    js_sys::Reflect::set(&device, &"deviceId".into(), &id.into()).unwrap();
    js_sys::Reflect::set(&device, &"kind".into(), &kind.into()).unwrap();
    js_sys::Reflect::set(&device, &"label".into(), &label.into()).unwrap();
    js_sys::Reflect::set(&device, &"groupId".into(), &"test-group".into()).unwrap();
    device.unchecked_into::<MediaDeviceInfo>()
}

/// Convenience wrappers for common device kinds.
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
// Real Chrome fake-device enumeration (Layer 3 tests)
// ---------------------------------------------------------------------------

/// Call `getUserMedia` (auto-granted by `--use-fake-ui-for-media-stream`)
/// then `enumerateDevices` to obtain *real* `MediaDeviceInfo` objects from
/// Chrome's fake-device infrastructure.
///
/// Returns `(microphones, cameras, speakers)`.
pub async fn enumerate_fake_devices() -> (
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
    Vec<MediaDeviceInfo>,
) {
    let navigator = gloo_utils::window().navigator();
    let media_devices = navigator.media_devices().expect("media_devices API");

    // Request a stream to trigger the permission grant (auto-accepted).
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

    // Now enumerate — labels will be populated because permission was granted.
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
