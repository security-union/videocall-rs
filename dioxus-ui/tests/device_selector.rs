// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Component rendering tests for DeviceSelector, DeviceSettingsModal and Host's device settings (Dioxus).
//
// Uses constructed mock MediaDeviceInfo objects so we can control the exact
// device count, labels, and IDs.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use support::{
    cleanup, create_mount_point, mock_camera, mock_mic, mock_speaker, render_into,
    wait_for_selector, yield_now,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use dioxus::prelude::*;
use dioxus_ui::components::attendants::PreAcquiredScreenStream;
use dioxus_ui::components::device_selector::DeviceSelector;
use dioxus_ui::components::device_settings_modal::DeviceSettingsModal;
use dioxus_ui::components::host::Host;
use dioxus_ui::components::share_actions::ScreenReplacementCell;
use dioxus_ui::context::{TransportPreference, TransportPreferenceCtx};
use videocall_client::VideoCallClient;

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
        mount
            .query_selector(".device-settings-modal")
            .unwrap()
            .is_none(),
        "modal should not render its content when visible=false"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_renders_audio_section_dropdowns_when_visible() {
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
        "audio select should be visible in default Audio section"
    );
    assert!(
        mount
            .query_selector("#modal-speaker-select")
            .unwrap()
            .is_some(),
        "speaker select should be visible in default Audio section"
    );
    assert!(
        mount
            .query_selector("#modal-video-select")
            .unwrap()
            .is_none(),
        "video select should not be visible until Video section is opened"
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

    let btn = mount
        .query_selector(".settings-modal-close")
        .unwrap()
        .unwrap();

    assert_eq!(
        btn.get_attribute("type").unwrap_or_default(),
        "button",
        "close button should have type=button"
    );
    assert_eq!(
        btn.get_attribute("aria-label").unwrap_or_default(),
        "Close settings",
        "close button should expose an accessible label"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_defaults_to_audio_section() {
    let mount = create_mount_point();

    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1")];
        let cams = vec![mock_camera("c1", "Cam 1")];
        let spks = vec![mock_speaker("s1", "Speaker 1")];

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

    let audio_btn = mount
        .query_selector("[data-testid='settings-nav-audio']")
        .unwrap()
        .unwrap();

    assert!(
        audio_btn.class_list().contains("active"),
        "Audio section should be active by default"
    );

    assert!(
        mount
            .query_selector("#modal-audio-select")
            .unwrap()
            .is_some(),
        "microphone select should be visible in Audio section"
    );

    assert!(
        mount
            .query_selector("#modal-speaker-select")
            .unwrap()
            .is_some(),
        "speaker select should be visible in Audio section"
    );

    assert!(
        mount
            .query_selector("#modal-video-select")
            .unwrap()
            .is_none(),
        "camera select should not be visible in Audio section"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_switches_to_video_section_on_click() {
    let mount = create_mount_point();

    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1")];
        let cams = vec![mock_camera("c1", "Cam 1")];
        let spks = vec![mock_speaker("s1", "Speaker 1")];

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

    let video_btn = mount
        .query_selector("[data-testid='settings-nav-video']")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();

    video_btn.click();
    yield_now().await;

    assert!(
        mount
            .query_selector("#modal-video-select")
            .unwrap()
            .is_some(),
        "camera select should be visible after switching to Video section"
    );

    assert!(
        mount
            .query_selector("#modal-audio-select")
            .unwrap()
            .is_none(),
        "microphone select should not be visible in Video section"
    );

    assert!(
        mount
            .query_selector("#modal-speaker-select")
            .unwrap()
            .is_none(),
        "speaker select should not be visible in Video section"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_updates_active_nav_highlighting() {
    let mount = create_mount_point();

    fn wrapper() -> Element {
        let mics = vec![mock_mic("m1", "Mic 1")];
        let cams = vec![mock_camera("c1", "Cam 1")];
        let spks = vec![mock_speaker("s1", "Speaker 1")];

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

    let audio_btn = mount
        .query_selector("[data-testid='settings-nav-audio']")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();

    let video_btn = mount
        .query_selector("[data-testid='settings-nav-video']")
        .unwrap()
        .unwrap()
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap();

    assert!(
        audio_btn.class_list().contains("active"),
        "Audio should start active"
    );
    assert!(
        !video_btn.class_list().contains("active"),
        "Video should start inactive"
    );

    video_btn.click();
    yield_now().await;

    let audio_btn = mount
        .query_selector("[data-testid='settings-nav-audio']")
        .unwrap()
        .unwrap();

    let video_btn = mount
        .query_selector("[data-testid='settings-nav-video']")
        .unwrap()
        .unwrap();

    assert!(
        !audio_btn.class_list().contains("active"),
        "Audio should no longer be active after switching"
    );
    assert!(
        video_btn.class_list().contains("active"),
        "Video should be active after switching"
    );

    cleanup(&mount);
}

#[wasm_bindgen_test]
async fn device_settings_modal_renders_expected_navigation_tabs() {
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

    let nav_buttons = mount.query_selector_all(".settings-nav-button").unwrap();
    assert_eq!(
        nav_buttons.length(),
        6,
        "should render exactly six nav buttons (Audio, Video, Network, Appearance, Preferences, Profile)"
    );

    let text = mount.text_content().unwrap_or_default();
    assert!(text.contains("Audio"), "should render Audio nav");
    assert!(text.contains("Video"), "should render Video nav");
    assert!(text.contains("Network"), "should render Network nav");
    assert!(text.contains("Appearance"), "should render Appearance nav");
    assert!(
        text.contains("Preferences"),
        "should render Preferences nav"
    );
    assert!(
        !text.contains("Performance"),
        "should not render a Performance nav or any performance affordance (panel moved to the Diagnostics drawer, #1131)"
    );
    assert!(!text.contains("Devices"), "should not render Devices nav");
    assert!(text.contains("Profile"), "should render Profile nav");
    let last = nav_buttons
        .item(nav_buttons.length() - 1)
        .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.get_attribute("data-testid"));
    assert_eq!(
        last.as_deref(),
        Some("settings-nav-profile"),
        "Profile is the last tab"
    );
    assert!(!text.contains("Advanced"), "should not render Advanced nav");

    // The Performance tab was removed in #1131: the simulcast layer-bounds panel
    // (#989 P4/P5) now lives in the unified Performance & Diagnostics drawer, and
    // the modal must expose no performance affordance at all.
    assert!(
        mount
            .query_selector("[data-testid='settings-nav-performance']")
            .unwrap()
            .is_none(),
        "Performance nav button must NOT exist (moved to the Diagnostics drawer, #1131)"
    );
    cleanup(&mount);
}

const PREF_KEYS: [&str; 3] = [
    "vc_prejoin_camera_id",
    "vc_prejoin_mic_id",
    "vc_prejoin_speaker_id",
];

fn storage() -> web_sys::Storage {
    web_sys::window().unwrap().local_storage().unwrap().unwrap()
}

fn js(source: &str) {
    js_sys::Function::new_no_args(source)
        .call0(&wasm_bindgen::JsValue::NULL)
        .unwrap();
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

#[allow(non_snake_case)]
fn HostWithSettings() -> Element {
    let client = use_hook(|| VideoCallClient::new_for_test("local-user"));
    use_context_provider(|| client.clone());
    let transport = use_signal(|| TransportPreference::WebTransport);
    use_context_provider(|| TransportPreferenceCtx(transport));
    use_context_provider(|| -> PreAcquiredScreenStream { Rc::new(RefCell::new(None)) });
    use_context_provider(ScreenReplacementCell::default);
    let mut mounted = use_signal(|| true);
    let mut settings_open = use_signal(|| false);
    rsx! {
        button { "data-testid": "open-settings", onclick: move |_| settings_open.set(true) }
        button { "data-testid": "unmount-host", onclick: move |_| mounted.set(false) }
        if mounted() {
            Host {
                share_screen: false,
                mic_enabled: false,
                video_enabled: false,
                on_encoder_settings_update: move |_| {},
                device_settings_open: settings_open(),
                on_device_settings_toggle: move |_| {},
                on_edit_avatar: move |_| {},
                device_settings_initial_section: Some("video".to_string()),
                on_screen_share_state: move |_| {},
                reload_devices_counter: 0,
                self_hidden: true,
                in_call: true,
            }
        }
    }
}

fn shown_label(mount: &web_sys::Element, select_id: &str) -> String {
    mount
        .query_selector(&format!("#{select_id} .glass-select-label"))
        .unwrap()
        .unwrap_or_else(|| panic!("#{select_id} should be rendered"))
        .text_content()
        .unwrap_or_default()
}

async fn click(mount: &web_sys::Element, selector: &str) {
    mount
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("{selector} should be rendered"))
        .dyn_into::<web_sys::HtmlElement>()
        .unwrap()
        .click();
    yield_now().await;
}

async fn pick(mount: &web_sys::Element, select_id: &str, label: &str) -> String {
    click(mount, &format!("#{select_id}")).await;
    let options = mount.query_selector_all(".glass-select-option").unwrap();
    let option = (0..options.length())
        .filter_map(|i| options.item(i))
        .find(|o| o.text_content().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("{select_id} has no option {label}"));
    option.dyn_into::<web_sys::HtmlElement>().unwrap().click();
    yield_now().await;
    sleep_ms(50).await;
    shown_label(mount, select_id)
}

#[wasm_bindgen_test]
async fn host_settings_show_an_in_call_device_pick_at_once() {
    for key in PREF_KEYS {
        let _ = storage().remove_item(key);
    }
    js(
        "const d = (deviceId, kind, label) => ({ deviceId, kind, label, groupId: 'g' });
        navigator.mediaDevices.enumerateDevices = () => Promise.resolve([
            d('cam-a', 'videoinput', 'Camera A'), d('cam-b', 'videoinput', 'Camera B'),
            d('mic-a', 'audioinput', 'Mic A'), d('mic-b', 'audioinput', 'Mic B'),
            d('spk-a', 'audiooutput', 'Speaker A'), d('spk-b', 'audiooutput', 'Speaker B'),
        ]);",
    );
    let mount = create_mount_point();
    render_into(&mount, HostWithSettings);
    yield_now().await;
    let mut waited = 0;
    while storage().get_item(PREF_KEYS[0]).unwrap().is_none() && waited < 2000 {
        sleep_ms(50).await;
        waited += 50;
    }

    click(&mount, "[data-testid='open-settings']").await;
    assert!(wait_for_selector(&mount, "#modal-video-select", 2000).await);
    assert_eq!(shown_label(&mount, "modal-video-select"), "Camera A");

    let camera = pick(&mount, "modal-video-select", "Camera B").await;
    click(&mount, "[data-testid='settings-nav-audio']").await;
    let microphone = pick(&mount, "modal-audio-select", "Mic B").await;
    let speaker = pick(&mount, "modal-speaker-select", "Speaker B").await;

    let stored: Vec<Option<String>> = PREF_KEYS
        .iter()
        .map(|key| storage().get_item(key).unwrap())
        .collect();
    assert_eq!(
        stored,
        [
            Some("cam-b".to_string()),
            Some("mic-b".to_string()),
            Some("spk-b".to_string()),
        ],
        "the picks did not reach Host"
    );
    assert_eq!(
        [camera, microphone, speaker],
        ["Camera B", "Mic B", "Speaker B"],
        "Settings kept showing the previous device"
    );

    click(&mount, "[data-testid='unmount-host']").await;
    cleanup(&mount);
    js("delete navigator.mediaDevices.enumerateDevices;");
    for key in PREF_KEYS {
        let _ = storage().remove_item(key);
    }
}
