// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component rendering tests for DeviceSelector and DeviceSettingsModal (Dioxus).
//
// Uses constructed mock MediaDeviceInfo objects so we can control the exact
// device count, labels, and IDs.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use support::{cleanup, create_mount_point, mock_camera, mock_mic, mock_speaker, render_into, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::device_selector::DeviceSelector;
use dioxus_ui::components::device_settings_modal::DeviceSettingsModal;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ===========================================================================
// DeviceSelector tests
// ===========================================================================

#[wasm_bindgen_test]
async fn device_selector_renders_all_three_dropdowns() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1")];
        let cams = vec![mock_camera("c1", "Camera 1")];
        let spks = vec![mock_speaker("s1", "Speaker 1")];
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    assert!(
        mount.query_selector("#audio-select").unwrap().is_some(),
        "audio dropdown"
    );
    assert!(
        mount.query_selector("#video-select").unwrap().is_some(),
        "video dropdown"
    );
    assert!(
        mount.query_selector("#speaker-select").unwrap().is_some(),
        "speaker dropdown"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_selector_renders_multiple_device_labels() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        let mics = vec![
            mock_mic("m1", "USB Mic"),
            mock_mic("m2", "Built-in Mic"),
            mock_mic("m3", "Bluetooth Headset"),
        ];
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let select = mount
        .query_selector("#audio-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();

    assert_eq!(select.options().length(), 3, "should render 3 options");

    let labels: Vec<String> = (0..select.options().length())
        .map(|i| {
            select
                .options()
                .item(i)
                .unwrap()
                .text_content()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(labels, vec!["USB Mic", "Built-in Mic", "Bluetooth Headset"]);

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_selector_preselects_correct_device() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1"), mock_mic("m2", "Mic 2")];
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: Some("m2".to_string()),
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let opt2 = mount
        .query_selector("#audio-select option[value='m2']")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlOptionElement>()
        .unwrap();
    assert!(opt2.selected(), "option m2 should be pre-selected");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_selector_empty_list_renders_empty_dropdown() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! {
            DeviceSelector {
                microphones: Vec::<web_sys::MediaDeviceInfo>::new(),
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let select = mount
        .query_selector("#audio-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    assert_eq!(
        select.options().length(),
        0,
        "no options when device list is empty"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_selector_empty_labels_render_empty_option_text() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "")];
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let opt = mount
        .query_selector("#audio-select option")
        .unwrap()
        .unwrap();
    assert_eq!(
        opt.text_content().unwrap_or_default(),
        "",
        "option text should be empty when device label is empty"
    );

    cleanup(&mount);
}

// ===========================================================================
// DeviceSettingsModal tests
// ===========================================================================

#[wasm_bindgen_test]
async fn device_settings_modal_hidden_when_not_visible() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! {
            DeviceSettingsModal {
                microphones: Vec::<web_sys::MediaDeviceInfo>::new(),
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
                visible: false,
                on_close: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    // Dioxus may render placeholder nodes (e.g. empty comment) for rsx! {}.
    // The key assertion is that no modal content is rendered.
    assert!(
        mount.query_selector(".device-settings-modal").unwrap().is_none(),
        "modal should not render its content when visible=false"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_renders_dropdowns_when_visible() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1")];
        let cams = vec![mock_camera("c1", "Cam 1")];
        let spks = vec![mock_speaker("s1", "Spk 1")];
        rsx! {
            DeviceSettingsModal {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
                visible: true,
                on_close: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    assert!(
        mount
            .query_selector("#modal-audio-select")
            .unwrap()
            .is_some(),
        "audio select"
    );
    assert!(
        mount
            .query_selector("#modal-video-select")
            .unwrap()
            .is_some(),
        "video select"
    );
    assert!(
        mount
            .query_selector("#modal-speaker-select")
            .unwrap()
            .is_some(),
        "speaker select"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_close_button_present() {
    let mount = create_mount_point();
    fn wrapper() -> Element {
        rsx! {
            DeviceSettingsModal {
                microphones: Vec::<web_sys::MediaDeviceInfo>::new(),
                cameras: Vec::<web_sys::MediaDeviceInfo>::new(),
                speakers: Vec::<web_sys::MediaDeviceInfo>::new(),
                selected_microphone_id: None::<String>,
                selected_camera_id: None::<String>,
                selected_speaker_id: None::<String>,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
                visible: true,
                on_close: move |_| {},
            }
        }
    }
    render_into(&mount, wrapper);
    yield_now().await;

    let btn = mount.query_selector(".close-button").unwrap().unwrap();
    assert_eq!(
        btn.text_content().unwrap(),
        "\u{00d7}",
        "close button should display multiplication sign"
    );

    cleanup(&mount);
}
