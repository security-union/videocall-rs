/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Host component - manages local encoders for camera, microphone, and screen sharing

use dioxus::prelude::*;
use futures::channel::mpsc;
use gloo_timers::callback::Timeout;
use std::cell::RefCell;
use std::rc::Rc;
use videocall_client::{
    create_microphone_encoder, CameraEncoder, MediaDeviceList, MicrophoneEncoderTrait,
    ScreenEncoder, ScreenShareEvent,
};
use videocall_types::protos::media_packet::media_packet::MediaType;

use crate::components::device_selector::DeviceSelector;
use crate::components::device_settings_modal::DeviceSettingsModal;
use crate::constants::{audio_bitrate_kbps, screen_bitrate_kbps, video_bitrate_kbps};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, VideoCallClientCtx,
};
use crate::types::DeviceInfo;

const VIDEO_ELEMENT_ID: &str = "webcam";

#[derive(Clone, Default)]
pub struct EncoderSettings {
    pub camera: Option<String>,
    pub microphone: Option<String>,
    pub screen: Option<String>,
}

impl std::fmt::Display for EncoderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut settings = Vec::new();
        if let Some(camera) = &self.camera {
            settings.push(format!("Camera: {camera}"));
        }
        if let Some(microphone) = &self.microphone {
            settings.push(format!("Microphone: {microphone}"));
        }
        if let Some(screen) = &self.screen {
            settings.push(format!("Screen: {screen}"));
        }
        write!(f, "{}", settings.join(", "))
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct HostProps {
    #[props(default)]
    pub id: String,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub on_encoder_settings_update: EventHandler<String>,
    pub device_settings_open: bool,
    pub on_device_settings_toggle: EventHandler<()>,
    #[props(default)]
    pub on_microphone_error: EventHandler<String>,
    #[props(default)]
    pub on_camera_error: EventHandler<String>,
    pub on_screen_share_state: EventHandler<ScreenShareEvent>,
}

/// Internal state holder for encoders
struct HostEncoders {
    camera: CameraEncoder,
    microphone: Box<dyn MicrophoneEncoderTrait>,
    screen: ScreenEncoder,
    media_devices: MediaDeviceList,
}

#[component]
pub fn Host(props: HostProps) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    // State for encoder settings
    let mut encoder_settings = use_signal(EncoderSettings::default);

    // State for device selections
    let mut selected_microphone_id = use_signal(String::new);
    let mut selected_camera_id = use_signal(String::new);
    let mut selected_speaker_id = use_signal(String::new);

    // State for device lists
    let mut microphones = use_signal(Vec::new);
    let mut cameras = use_signal(Vec::new);
    let mut speakers = use_signal(Vec::new);

    // State for change name modal
    let mut show_change_name = use_signal(|| false);
    let mut pending_name = use_signal(String::new);
    let mut change_name_error = use_signal(|| None::<String>);

    // Track encoder state
    let mut mic_enabled = use_signal(|| props.mic_enabled);
    let mut video_enabled = use_signal(|| props.video_enabled);
    let mut share_screen = use_signal(|| props.share_screen);

    // Encoders reference (stored in Rc<RefCell> for sharing across closures)
    let encoders: Signal<Option<Rc<RefCell<HostEncoders>>>> = use_signal(|| None);

    // Initialize encoders on mount
    let on_encoder_settings_update = props.on_encoder_settings_update.clone();
    let on_screen_share_state = props.on_screen_share_state.clone();
    let on_microphone_error = props.on_microphone_error.clone();
    let on_camera_error = props.on_camera_error.clone();

    use_effect(move || {
        if let Some(ref client) = client {
            // Create encoder callbacks using Yew callbacks (videocall-client requires them)
            let camera_callback = {
                let on_update = on_encoder_settings_update.clone();
                yew::Callback::from(move |settings: String| {
                    encoder_settings.write().camera = Some(settings.clone());
                    on_update.call(encoder_settings.read().to_string());
                })
            };

            let microphone_callback = {
                let on_update = on_encoder_settings_update.clone();
                yew::Callback::from(move |settings: String| {
                    encoder_settings.write().microphone = Some(settings.clone());
                    on_update.call(encoder_settings.read().to_string());
                })
            };

            let screen_callback = {
                let on_update = on_encoder_settings_update.clone();
                yew::Callback::from(move |settings: String| {
                    encoder_settings.write().screen = Some(settings.clone());
                    on_update.call(encoder_settings.read().to_string());
                })
            };

            let camera_error_cb = {
                let on_error = on_camera_error.clone();
                yew::Callback::from(move |err: String| {
                    on_error.call(err);
                })
            };

            let microphone_error_cb = {
                let on_error = on_microphone_error.clone();
                yew::Callback::from(move |err: String| {
                    on_error.call(err);
                })
            };

            let screen_state_callback = {
                let on_state = on_screen_share_state.clone();
                yew::Callback::from(move |event: ScreenShareEvent| {
                    on_state.call(event);
                })
            };

            // Create encoders
            let video_bitrate = video_bitrate_kbps().unwrap_or(1000);
            let mut camera = CameraEncoder::new(
                client.clone(),
                VIDEO_ELEMENT_ID,
                video_bitrate,
                camera_callback,
                camera_error_cb,
            );

            let audio_bitrate = audio_bitrate_kbps().unwrap_or(65);
            let mut microphone = create_microphone_encoder(
                client.clone(),
                audio_bitrate,
                microphone_callback,
                microphone_error_cb,
            );

            let screen_bitrate = screen_bitrate_kbps().unwrap_or(1000);
            let mut screen = ScreenEncoder::new(
                client.clone(),
                screen_bitrate,
                screen_callback,
                screen_state_callback,
            );

            // Subscribe to diagnostics for encoder control
            let (tx, rx) = mpsc::unbounded();
            client.subscribe_diagnostics(tx.clone(), MediaType::VIDEO);
            camera.set_encoder_control(rx);

            let (tx, rx) = mpsc::unbounded();
            client.subscribe_diagnostics(tx.clone(), MediaType::AUDIO);
            microphone.set_encoder_control(rx);

            let (tx, rx) = mpsc::unbounded();
            client.subscribe_diagnostics(tx.clone(), MediaType::SCREEN);
            screen.set_encoder_control(rx);

            // Create and configure MediaDeviceList
            let mut media_devices = MediaDeviceList::new();

            media_devices.on_loaded = {
                yew::Callback::from(move |_| {
                    log::info!("Devices loaded");
                })
            };

            media_devices.on_devices_changed = {
                yew::Callback::from(move |_| {
                    log::info!("Devices changed");
                })
            };

            // Load devices
            media_devices.load();

            // Update device lists
            microphones.set(media_devices.audio_inputs.devices());
            cameras.set(media_devices.video_inputs.devices());
            speakers.set(media_devices.audio_outputs.devices());

            selected_microphone_id.set(media_devices.audio_inputs.selected());
            selected_camera_id.set(media_devices.video_inputs.selected());
            selected_speaker_id.set(media_devices.audio_outputs.selected());

            // Store encoders
            let host_encoders = HostEncoders {
                camera,
                microphone,
                screen,
                media_devices,
            };
            encoders.set(Some(Rc::new(RefCell::new(host_encoders))));
        }
    });

    // Update encoder states based on props
    use_effect(move || {
        if let Some(ref enc) = *encoders.read() {
            let mut enc = enc.borrow_mut();

            // Update screen share
            if props.share_screen != *share_screen.read() {
                share_screen.set(props.share_screen);
                if props.share_screen {
                    enc.screen.set_enabled(true);
                    enc.screen.start();
                } else {
                    enc.screen.stop();
                    enc.screen.set_enabled(false);
                }
            }

            // Update mic
            if props.mic_enabled != *mic_enabled.read() {
                mic_enabled.set(props.mic_enabled);
                if props.mic_enabled {
                    enc.microphone.set_enabled(true);
                    enc.microphone.start();
                } else {
                    enc.microphone.stop();
                    enc.microphone.set_enabled(false);
                }
            }

            // Update video
            if props.video_enabled != *video_enabled.read() {
                video_enabled.set(props.video_enabled);
                if props.video_enabled {
                    enc.camera.set_enabled(true);
                    enc.camera.start();
                } else {
                    enc.camera.stop();
                    enc.camera.set_enabled(false);
                }
            }

            // Update client states
            if let Some(ref client) = client {
                client.set_audio_enabled(props.mic_enabled);
                client.set_video_enabled(props.video_enabled);
            }
        }
    });

    // Device change handlers
    let on_microphone_change = move |device: DeviceInfo| {
        log::info!("Microphone changed: {:?}", device);
        if let Some(ref enc) = *encoders.read() {
            let mut enc = enc.borrow_mut();
            enc.media_devices.audio_inputs.select(&device.device_id);
            if enc.microphone.select(device.device_id.clone()) {
                // Restart microphone after a delay
                let enc_clone = encoders.read().clone();
                Timeout::new(1000, move || {
                    if let Some(ref enc) = enc_clone {
                        enc.borrow_mut().microphone.start();
                    }
                })
                .forget();
            }
        }
        selected_microphone_id.set(device.device_id);
    };

    let on_camera_change = move |device: DeviceInfo| {
        log::info!("Camera changed: {:?}", device);
        if let Some(ref enc) = *encoders.read() {
            let mut enc = enc.borrow_mut();
            enc.media_devices.video_inputs.select(&device.device_id);
            if enc.camera.select(device.device_id.clone()) {
                // Restart camera after a delay
                let enc_clone = encoders.read().clone();
                Timeout::new(1000, move || {
                    if let Some(ref enc) = enc_clone {
                        enc.borrow_mut().camera.start();
                    }
                })
                .forget();
            }
        }
        selected_camera_id.set(device.device_id);
    };

    let on_speaker_change = move |device: DeviceInfo| {
        log::info!("Speaker changed: {:?}", device);
        if let Some(ref enc) = *encoders.read() {
            enc.borrow_mut()
                .media_devices
                .audio_outputs
                .select(&device.device_id);
        }
        if let Some(ref client) = client {
            if let Err(e) = client.update_speaker_device(Some(device.device_id.clone())) {
                log::error!("Failed to update speaker device: {e:?}");
            }
        }
        selected_speaker_id.set(device.device_id);
    };

    // Change name handlers
    let open_change_name = move |_| {
        pending_name.set(load_username_from_storage().unwrap_or_default());
        show_change_name.set(true);
        change_name_error.set(None);
    };

    let cancel_change_name = move |_| {
        show_change_name.set(false);
        change_name_error.set(None);
    };

    let save_change_name = move |_| {
        let new_name = pending_name.read().trim().to_string();
        if is_valid_username(&new_name) && !new_name.is_empty() {
            save_username_to_storage(&new_name);
            if let Some(win) = web_sys::window() {
                let _ = win.location().reload();
            }
            show_change_name.set(false);
            change_name_error.set(None);
        } else {
            change_name_error.set(Some("Use letters, numbers, and underscore only.".to_string()));
        }
    };

    rsx! {
        // Video preview or placeholder
        if props.video_enabled {
            div { class: "host-video-wrapper", style: "position:relative;",
                video {
                    class: "self-camera",
                    autoplay: true,
                    id: VIDEO_ELEMENT_ID,
                    playsinline: true,
                    controls: false
                }
                button {
                    class: "change-name-fab",
                    title: "Change name",
                    onclick: open_change_name,
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        path { d: "M12 20h9" }
                        path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z" }
                    }
                }
            }
        } else {
            div {
                class: "",
                style: "padding:1rem; display:flex; align-items:center; justify-content:center; border-radius: 1rem; position:relative;",
                div { class: "placeholder-content",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    }
                    span { class: "placeholder-text", "Camera Off" }
                }
                button {
                    class: "change-name-fab",
                    title: "Change name",
                    onclick: open_change_name,
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        path { d: "M12 20h9" }
                        path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z" }
                    }
                }
            }
        }

        // Device Settings Menu Button
        button {
            class: "device-settings-menu-button btn-apple btn-secondary",
            onclick: move |_| props.on_device_settings_toggle.call(()),
            title: "Device Settings",
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "3" }
                path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
            }
        }

        // Desktop Device Selector
        div { class: "desktop-device-selector",
            DeviceSelector {
                microphones: microphones.read().clone(),
                cameras: cameras.read().clone(),
                speakers: speakers.read().clone(),
                selected_microphone_id: selected_microphone_id.read().clone(),
                selected_camera_id: selected_camera_id.read().clone(),
                selected_speaker_id: selected_speaker_id.read().clone(),
                on_microphone_select: on_microphone_change,
                on_camera_select: on_camera_change,
                on_speaker_select: on_speaker_change
            }
        }

        // Mobile Device Settings Modal
        DeviceSettingsModal {
            microphones: microphones.read().clone(),
            cameras: cameras.read().clone(),
            speakers: speakers.read().clone(),
            selected_microphone_id: selected_microphone_id.read().clone(),
            selected_camera_id: selected_camera_id.read().clone(),
            selected_speaker_id: selected_speaker_id.read().clone(),
            on_microphone_select: on_microphone_change,
            on_camera_select: on_camera_change,
            on_speaker_select: on_speaker_change,
            visible: props.device_settings_open,
            on_close: move |_| props.on_device_settings_toggle.call(())
        }

        // Change Name Modal
        if *show_change_name.read() {
            div { class: "glass-backdrop",
                div { class: "card-apple", style: "width: 380px;",
                    h3 { style: "margin-top:0;", "Change your name" }
                    p { style: "color:#AEAEB2; margin-top:0.25rem;",
                        "This name will be visible to others in the meeting."
                    }
                    input {
                        class: "input-apple",
                        value: "{pending_name}",
                        oninput: move |evt| pending_name.set(evt.value()),
                        placeholder: "Enter new name",
                        pattern: "^[a-zA-Z0-9_]*$",
                        autofocus: true
                    }
                    if let Some(err) = change_name_error.read().as_ref() {
                        p { style: "color:#FF453A; margin-top:6px; font-size:12px;", "{err}" }
                    }
                    div { style: "display:flex; gap:8px; justify-content:flex-end; margin-top:12px;",
                        button {
                            class: "btn-apple btn-secondary btn-sm",
                            onclick: cancel_change_name,
                            "Cancel"
                        }
                        button {
                            class: "btn-apple btn-primary btn-sm",
                            onclick: save_change_name,
                            "Save"
                        }
                    }
                }
            }
        }
    }
}
