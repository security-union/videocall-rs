// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component rendering tests for DeviceSelector.
//
// Uses constructed mock MediaDeviceInfo objects so we can control the exact
// device count, labels, and IDs.  This is the correct pattern for component
// isolation tests where Chrome's single fake device is not enough.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use support::{cleanup, create_mount_point, mock_camera, mock_mic, mock_speaker, mount_dioxus, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

use dioxus_ui::components::device_selector::DeviceSelector;
use dioxus_ui::types::DeviceInfo;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ---------------------------------------------------------------------------
// thread_local for passing test data to wrapper functions
// ---------------------------------------------------------------------------

thread_local! {
    static TEST_MICS: RefCell<Vec<web_sys::MediaDeviceInfo>> = RefCell::new(Vec::new());
    static TEST_CAMS: RefCell<Vec<web_sys::MediaDeviceInfo>> = RefCell::new(Vec::new());
    static TEST_SPKS: RefCell<Vec<web_sys::MediaDeviceInfo>> = RefCell::new(Vec::new());
    static TEST_SELECTED_MIC: RefCell<Option<String>> = RefCell::new(None);
    static TEST_SELECTED_CAM: RefCell<Option<String>> = RefCell::new(None);
    static TEST_SELECTED_SPK: RefCell<Option<String>> = RefCell::new(None);
    static TEST_RECEIVED: RefCell<Option<DeviceInfo>> = RefCell::new(None);
}

fn set_test_devices(
    mics: Vec<web_sys::MediaDeviceInfo>,
    cams: Vec<web_sys::MediaDeviceInfo>,
    spks: Vec<web_sys::MediaDeviceInfo>,
) {
    TEST_MICS.with(|m| *m.borrow_mut() = mics);
    TEST_CAMS.with(|c| *c.borrow_mut() = cams);
    TEST_SPKS.with(|s| *s.borrow_mut() = spks);
}

fn set_test_selections(mic: Option<String>, cam: Option<String>, spk: Option<String>) {
    TEST_SELECTED_MIC.with(|m| *m.borrow_mut() = mic);
    TEST_SELECTED_CAM.with(|c| *c.borrow_mut() = cam);
    TEST_SELECTED_SPK.with(|s| *s.borrow_mut() = spk);
}

fn clear_received() {
    TEST_RECEIVED.with(|r| *r.borrow_mut() = None);
}

fn get_received() -> Option<DeviceInfo> {
    TEST_RECEIVED.with(|r| r.borrow().clone())
}

// ===========================================================================
// DeviceSelector tests
// ===========================================================================

#[wasm_bindgen_test]
async fn device_selector_renders_all_three_dropdowns() {
    set_test_devices(
        vec![mock_mic("m1", "Mic 1")],
        vec![mock_camera("c1", "Camera 1")],
        vec![mock_speaker("s1", "Speaker 1")],
    );
    set_test_selections(None, None, None);

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    set_test_devices(
        vec![
            mock_mic("m1", "USB Mic"),
            mock_mic("m2", "Built-in Mic"),
            mock_mic("m3", "Bluetooth Headset"),
        ],
        vec![],
        vec![],
    );
    set_test_selections(None, None, None);

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    set_test_devices(
        vec![mock_mic("m1", "Mic 1"), mock_mic("m2", "Mic 2")],
        vec![],
        vec![],
    );
    set_test_selections(Some("m2".to_string()), None, None);

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    set_test_devices(vec![], vec![], vec![]);
    set_test_selections(None, None, None);

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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
    set_test_devices(vec![mock_mic("m1", "")], vec![], vec![]);
    set_test_selections(None, None, None);

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

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

#[wasm_bindgen_test]
async fn device_selector_onchange_fires_microphone_callback() {
    set_test_devices(
        vec![mock_mic("m1", "Mic 1"), mock_mic("m2", "Mic 2")],
        vec![],
        vec![],
    );
    set_test_selections(None, None, None);
    clear_received();

    fn wrapper() -> Element {
        let mics = TEST_MICS.with(|m| m.borrow().clone());
        let cams = TEST_CAMS.with(|c| c.borrow().clone());
        let spks = TEST_SPKS.with(|s| s.borrow().clone());
        let sel_mic = TEST_SELECTED_MIC.with(|m| m.borrow().clone());
        let sel_cam = TEST_SELECTED_CAM.with(|c| c.borrow().clone());
        let sel_spk = TEST_SELECTED_SPK.with(|s| s.borrow().clone());
        rsx! {
            DeviceSelector {
                microphones: mics,
                cameras: cams,
                speakers: spks,
                selected_microphone_id: sel_mic,
                selected_camera_id: sel_cam,
                selected_speaker_id: sel_spk,
                on_microphone_select: move |info: DeviceInfo| {
                    TEST_RECEIVED.with(|r| *r.borrow_mut() = Some(info));
                },
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
            }
        }
    }

    let mount = create_mount_point();
    mount_dioxus(wrapper, &mount).await;

    // Programmatically change the select value and dispatch a change event
    let select = mount
        .query_selector("#audio-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    select.set_value("m2");
    let event = web_sys::Event::new("change").unwrap();
    select.dispatch_event(&event).unwrap();
    yield_now().await;

    let info = get_received();
    assert!(info.is_some(), "callback should have been called");
    assert_eq!(info.as_ref().unwrap().device_id, "m2");

    cleanup(&mount);
}
