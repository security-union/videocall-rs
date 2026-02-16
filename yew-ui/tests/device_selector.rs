// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component rendering tests for DeviceSelector and DeviceSettingsModal.
//
// Uses constructed mock MediaDeviceInfo objects so we can control the exact
// device count, labels, and IDs.  This is the correct pattern for component
// isolation tests where Chrome's single fake device is not enough.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use support::{cleanup, create_mount_point, mock_camera, mock_mic, mock_speaker};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use yew::platform::time::sleep;
use yew::prelude::*;

use videocall_ui::components::device_selector::DeviceSelector;
use videocall_ui::components::device_settings_modal::DeviceSettingsModal;
use videocall_ui::types::DeviceInfo;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// ===========================================================================
// DeviceSelector tests
// ===========================================================================

#[wasm_bindgen_test]
async fn device_selector_renders_all_three_dropdowns() {
    let mics = vec![mock_mic("m1", "Mic 1")];
    let cams = vec![mock_camera("c1", "Camera 1")];
    let spks = vec![mock_speaker("s1", "Speaker 1")];

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
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics, cams, spks })
        .render();
    sleep(Duration::ZERO).await;

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
    let mics = vec![
        mock_mic("m1", "USB Mic"),
        mock_mic("m2", "Built-in Mic"),
        mock_mic("m3", "Bluetooth Headset"),
    ];

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
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
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics }).render();
    sleep(Duration::ZERO).await;

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
    let mics = vec![mock_mic("m1", "Mic 1"), mock_mic("m2", "Mic 2")];

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
                selected_microphone_id={Some("m2".to_string())}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics }).render();
    sleep(Duration::ZERO).await;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <DeviceSelector
                microphones={Vec::<web_sys::MediaDeviceInfo>::new()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
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
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

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
    let mics = vec![mock_mic("m1", "")];

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
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
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics }).render();
    sleep(Duration::ZERO).await;

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
    let mics = vec![mock_mic("m1", "Mic 1"), mock_mic("m2", "Mic 2")];

    let received = Rc::new(RefCell::new(None::<DeviceInfo>));
    let received_c = received.clone();

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
        cb: Callback<DeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={props.mics.clone()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={props.cb.clone()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
            />
        }
    }

    let cb = Callback::from(move |info: DeviceInfo| {
        *received_c.borrow_mut() = Some(info);
    });

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics, cb }).render();
    sleep(Duration::ZERO).await;

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
    sleep(Duration::ZERO).await;

    let info = received.borrow();
    assert!(info.is_some(), "callback should have been called");
    assert_eq!(info.as_ref().unwrap().device_id, "m2");

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_selector_onchange_fires_camera_callback() {
    let cams = vec![mock_camera("c1", "Cam 1"), mock_camera("c2", "Cam 2")];

    let received = Rc::new(RefCell::new(None::<DeviceInfo>));
    let received_c = received.clone();

    #[derive(Properties, PartialEq)]
    struct Props {
        cams: Vec<web_sys::MediaDeviceInfo>,
        cb: Callback<DeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSelector
                microphones={Vec::<web_sys::MediaDeviceInfo>::new()}
                cameras={props.cams.clone()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={props.cb.clone()}
                on_speaker_select={Callback::noop()}
            />
        }
    }

    let cb = Callback::from(move |info: DeviceInfo| {
        *received_c.borrow_mut() = Some(info);
    });

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { cams, cb }).render();
    sleep(Duration::ZERO).await;

    let select = mount
        .query_selector("#video-select")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlSelectElement>()
        .unwrap();
    select.set_value("c2");
    let event = web_sys::Event::new("change").unwrap();
    select.dispatch_event(&event).unwrap();
    sleep(Duration::ZERO).await;

    let info = received.borrow();
    assert!(info.is_some(), "camera callback should have been called");
    assert_eq!(info.as_ref().unwrap().device_id, "c2");

    cleanup(&mount);
}

// ===========================================================================
// DeviceSettingsModal tests
// ===========================================================================

#[wasm_bindgen_test]
async fn device_settings_modal_hidden_when_not_visible() {
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <DeviceSettingsModal
                microphones={Vec::<web_sys::MediaDeviceInfo>::new()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
                visible={false}
                on_close={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    assert!(
        mount.inner_html().is_empty(),
        "modal should render nothing when visible=false"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_renders_dropdowns_when_visible() {
    let mics = vec![mock_mic("m1", "Mic 1")];
    let cams = vec![mock_camera("c1", "Cam 1")];
    let spks = vec![mock_speaker("s1", "Spk 1")];

    #[derive(Properties, PartialEq)]
    struct Props {
        mics: Vec<web_sys::MediaDeviceInfo>,
        cams: Vec<web_sys::MediaDeviceInfo>,
        spks: Vec<web_sys::MediaDeviceInfo>,
    }
    #[function_component(Wrapper)]
    fn wrapper(props: &Props) -> Html {
        html! {
            <DeviceSettingsModal
                microphones={props.mics.clone()}
                cameras={props.cams.clone()}
                speakers={props.spks.clone()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
                visible={true}
                on_close={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root_and_props(mount.clone(), Props { mics, cams, spks })
        .render();
    sleep(Duration::ZERO).await;

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
    #[function_component(Wrapper)]
    fn wrapper() -> Html {
        html! {
            <DeviceSettingsModal
                microphones={Vec::<web_sys::MediaDeviceInfo>::new()}
                cameras={Vec::<web_sys::MediaDeviceInfo>::new()}
                speakers={Vec::<web_sys::MediaDeviceInfo>::new()}
                selected_microphone_id={None::<String>}
                selected_camera_id={None::<String>}
                selected_speaker_id={None::<String>}
                on_microphone_select={Callback::noop()}
                on_camera_select={Callback::noop()}
                on_speaker_select={Callback::noop()}
                visible={true}
                on_close={Callback::noop()}
            />
        }
    }

    let mount = create_mount_point();
    yew::Renderer::<Wrapper>::with_root(mount.clone()).render();
    sleep(Duration::ZERO).await;

    let btn = mount.query_selector(".close-button").unwrap().unwrap();
    assert_eq!(
        btn.text_content().unwrap(),
        "\u{00d7}",
        "close button should display multiplication sign"
    );

    cleanup(&mount);
}
