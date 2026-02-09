// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Integration tests using real Chrome fake devices.
//
// These tests rely on `webdriver.json` configuring Chrome with
// `--use-fake-device-for-media-stream` and `--use-fake-ui-for-media-stream`.
// They call the real browser APIs to obtain genuine `MediaDeviceInfo` objects
// and verify the full pipeline through DeviceSelector rendering.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::time::Duration;

use support::{cleanup, create_mount_point, enumerate_fake_devices};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;

use videocall_ui::components::device_selector::DeviceSelector;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// Verify Chrome's fake device infrastructure works
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn enumerate_real_fake_devices_returns_labeled_devices() {
    let (mics, cams, _speakers) = enumerate_fake_devices().await;

    assert!(
        !mics.is_empty(),
        "Chrome should provide at least one fake audioinput device"
    );
    assert!(
        !cams.is_empty(),
        "Chrome should provide at least one fake videoinput device"
    );

    // Verify labels are populated (permission was auto-granted)
    let mic_label = mics[0].label();
    assert!(
        !mic_label.is_empty(),
        "fake mic label should not be empty, got: '{mic_label}'"
    );
    let cam_label = cams[0].label();
    assert!(
        !cam_label.is_empty(),
        "fake camera label should not be empty, got: '{cam_label}'"
    );
}

// ---------------------------------------------------------------------------
// Render DeviceSelector with real fake devices
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn device_selector_renders_real_fake_devices() {
    let (mics, cams, speakers) = enumerate_fake_devices().await;

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
        cams: Vec<web_sys::MediaDeviceInfo>,
        spks: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={props.cams.clone()}
                speakers={props.spks.clone()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(
        mount.clone(),
        Props {
            mics: mics.clone(),
            cams: cams.clone(),
            spks: speakers,
        },
    )
    .render();
    sleep(Duration::ZERO).await;

    // Audio dropdown should contain at least one option with a real label
    let audio_select = mount
        .query_selector("#audio-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    assert!(
        audio_select.options().length() >= 1,
        "audio dropdown should have at least 1 option from Chrome's fake devices"
    );
    let first_audio_opt = audio_select
        .options()
        .item(0)
        .unwrap()
        .dyn_into::<web_sys::HtmlOptionElement>()
        .unwrap();
    assert!(
        !first_audio_opt.text().is_empty(),
        "audio option should show the real fake device label"
    );

    // Video dropdown
    let video_select = mount
        .query_selector("#video-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    assert!(
        video_select.options().length() >= 1,
        "video dropdown should have at least 1 option from Chrome's fake devices"
    );

    cleanup(&mount);
}

// ---------------------------------------------------------------------------
// Verify option values match real device IDs
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn device_selector_real_device_ids_match_option_values() {
    let (mics, cams, speakers) = enumerate_fake_devices().await;

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
        cams: Vec<web_sys::MediaDeviceInfo>,
        spks: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={props.cams.clone()}
                speakers={props.spks.clone()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(
        mount.clone(),
        Props {
            mics: mics.clone(),
            cams: cams.clone(),
            spks: speakers,
        },
    )
    .render();
    sleep(Duration::ZERO).await;

    // Each mic's device_id should match the corresponding option's value
    let audio_select = mount
        .query_selector("#audio-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    for (i, mic) in mics.iter().enumerate() {
        let opt = audio_select
            .options()
            .item(i as u32)
            .unwrap()
            .dyn_into::<web_sys::HtmlOptionElement>()
            .unwrap();
        assert_eq!(
            opt.value(),
            mic.device_id(),
            "option value should match device_id for mic at index {i}"
        );
    }

    // Same for cameras
    let video_select = mount
        .query_selector("#video-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    for (i, cam) in cams.iter().enumerate() {
        let opt = video_select
            .options()
            .item(i as u32)
            .unwrap()
            .dyn_into::<web_sys::HtmlOptionElement>()
            .unwrap();
        assert_eq!(
            opt.value(),
            cam.device_id(),
            "option value should match device_id for camera at index {i}"
        );
    }

    cleanup(&mount);
}
