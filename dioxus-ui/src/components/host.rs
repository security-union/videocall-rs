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
    ScreenEncoder, ScreenShareEvent, VideoCallClient,
};
use videocall_types::protos::media_packet::media_packet::MediaType;

use crate::components::device_selector::DeviceSelector;
use crate::components::device_settings_modal::DeviceSettingsModal;
use crate::constants::{audio_bitrate_kbps, screen_bitrate_kbps, video_bitrate_kbps};
use crate::context::{is_valid_username, load_username_from_storage, save_username_to_storage};
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
    /// The VideoCallClient instance - required for encoders to work
    pub client: Option<VideoCallClient>,
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
    // Use client from props instead of context
    let client = props.client.clone();
    let client_for_init = client.clone();
    let client_for_speaker = client.clone();

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

    // Encoders reference (stored in Rc<RefCell> for sharing across closures)
    let mut encoders: Signal<Option<Rc<RefCell<HostEncoders>>>> = use_signal(|| None);

    // Initialize encoders on mount
    let on_encoder_settings_update = props.on_encoder_settings_update.clone();
    let on_screen_share_state = props.on_screen_share_state.clone();
    let on_microphone_error = props.on_microphone_error.clone();
    let on_camera_error = props.on_camera_error.clone();

    // Use Rc<RefCell> for encoder settings shared with Yew callbacks
    // (Yew's Callback::from requires Fn, not FnMut)
    let encoder_settings_rc = Rc::new(RefCell::new(EncoderSettings::default()));

    use_effect(move || {
        if let Some(ref client) = client_for_init {
            // Create encoder callbacks using Yew callbacks (videocall-client requires them)
            let camera_callback = {
                let on_update = on_encoder_settings_update.clone();
                let settings_rc = encoder_settings_rc.clone();
                yew::Callback::from(move |settings: String| {
                    settings_rc.borrow_mut().camera = Some(settings.clone());
                    on_update.call(settings_rc.borrow().to_string());
                })
            };

            let microphone_callback = {
                let on_update = on_encoder_settings_update.clone();
                let settings_rc = encoder_settings_rc.clone();
                yew::Callback::from(move |settings: String| {
                    settings_rc.borrow_mut().microphone = Some(settings.clone());
                    on_update.call(settings_rc.borrow().to_string());
                })
            };

            let screen_callback = {
                let on_update = on_encoder_settings_update.clone();
                let settings_rc = encoder_settings_rc.clone();
                yew::Callback::from(move |settings: String| {
                    settings_rc.borrow_mut().screen = Some(settings.clone());
                    on_update.call(settings_rc.borrow().to_string());
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

            // Create MediaDeviceList and wrap everything in Rc<RefCell> first,
            // so the on_loaded/on_devices_changed callbacks can access encoders.
            let media_devices = MediaDeviceList::new();
            let host_encoders_rc = Rc::new(RefCell::new(HostEncoders {
                camera,
                microphone,
                screen,
                media_devices,
            }));

            // Set up on_loaded callback: select default devices on encoders and update UI.
            // This mirrors Yew's Msg::DevicesLoaded handler — without this, start() silently
            // returns because state.selected is None.
            //
            // Note: yew::Callback::from requires Fn, but Signal::set takes &mut self (FnMut).
            // Workaround: copy the signal handle into a local `let mut` inside the closure
            // so only the local is mutated, keeping the closure Fn.
            {
                let enc_rc = host_encoders_rc.clone();
                let client_clone = client.clone();
                host_encoders_rc.borrow_mut().media_devices.on_loaded = {
                    yew::Callback::from(move |_| {
                        log::info!("Devices loaded");
                        let (speaker_id, restart_mic, restart_cam) = {
                            let mut enc = enc_rc.borrow_mut();

                            let mut mic_sig = microphones;
                            mic_sig.set(enc.media_devices.audio_inputs.devices());
                            let mut cam_sig = cameras;
                            cam_sig.set(enc.media_devices.video_inputs.devices());
                            let mut spk_sig = speakers;
                            spk_sig.set(enc.media_devices.audio_outputs.devices());

                            let audio_id = enc.media_devices.audio_inputs.selected();
                            let video_id = enc.media_devices.video_inputs.selected();
                            let speaker_id = enc.media_devices.audio_outputs.selected();

                            let mut sel_mic = selected_microphone_id;
                            sel_mic.set(audio_id.clone());
                            let mut sel_cam = selected_camera_id;
                            sel_cam.set(video_id.clone());
                            let mut sel_spk = selected_speaker_id;
                            sel_spk.set(speaker_id.clone());

                            // Select default devices on encoders (critical for start() to work)
                            let restart_mic = enc.microphone.select(audio_id);
                            let restart_cam = enc.camera.select(video_id);

                            (speaker_id, restart_mic, restart_cam)
                        };

                        if restart_mic {
                            let enc_rc2 = enc_rc.clone();
                            Timeout::new(1000, move || {
                                enc_rc2.borrow_mut().microphone.start();
                            })
                            .forget();
                        }
                        if restart_cam {
                            let enc_rc2 = enc_rc.clone();
                            Timeout::new(1000, move || {
                                enc_rc2.borrow_mut().camera.start();
                            })
                            .forget();
                        }

                        if let Err(e) = client_clone.update_speaker_device(Some(speaker_id)) {
                            log::error!("Failed to update speaker device: {e:?}");
                        }
                    })
                };
            }

            // Set up on_devices_changed callback (same logic as on_loaded)
            {
                let enc_rc = host_encoders_rc.clone();
                let client_clone = client.clone();
                host_encoders_rc.borrow_mut().media_devices.on_devices_changed = {
                    yew::Callback::from(move |_| {
                        log::info!("Devices changed");
                        let (speaker_id, restart_mic, restart_cam) = {
                            let mut enc = enc_rc.borrow_mut();

                            let mut mic_sig = microphones;
                            mic_sig.set(enc.media_devices.audio_inputs.devices());
                            let mut cam_sig = cameras;
                            cam_sig.set(enc.media_devices.video_inputs.devices());
                            let mut spk_sig = speakers;
                            spk_sig.set(enc.media_devices.audio_outputs.devices());

                            let audio_id = enc.media_devices.audio_inputs.selected();
                            let video_id = enc.media_devices.video_inputs.selected();
                            let speaker_id = enc.media_devices.audio_outputs.selected();

                            let mut sel_mic = selected_microphone_id;
                            sel_mic.set(audio_id.clone());
                            let mut sel_cam = selected_camera_id;
                            sel_cam.set(video_id.clone());
                            let mut sel_spk = selected_speaker_id;
                            sel_spk.set(speaker_id.clone());

                            let restart_mic = enc.microphone.select(audio_id);
                            let restart_cam = enc.camera.select(video_id);

                            (speaker_id, restart_mic, restart_cam)
                        };

                        if restart_mic {
                            let enc_rc2 = enc_rc.clone();
                            Timeout::new(1000, move || {
                                enc_rc2.borrow_mut().microphone.start();
                            })
                            .forget();
                        }
                        if restart_cam {
                            let enc_rc2 = enc_rc.clone();
                            Timeout::new(1000, move || {
                                enc_rc2.borrow_mut().camera.start();
                            })
                            .forget();
                        }

                        if let Err(e) = client_clone.update_speaker_device(Some(speaker_id)) {
                            log::error!("Failed to update speaker device: {e:?}");
                        }
                    })
                };
            }

            // Load devices (async — callbacks fire when enumeration completes)
            host_encoders_rc.borrow_mut().media_devices.load();

            // Store encoders in signal
            encoders.set(Some(host_encoders_rc));
        }
    });

    // Track previously-applied encoder state to detect prop transitions.
    // Using Rc<RefCell> (not a signal) to avoid any reactive subscription issues.
    let prev_encoder_state: Rc<RefCell<(bool, bool, bool)>> =
        use_hook(|| Rc::new(RefCell::new((false, false, false))));

    // Detect prop changes and apply encoder operations directly via spawn_local.
    // This mirrors Yew's `rendered()` lifecycle: runs every render, detects transitions,
    // and defers side effects to after the DOM update.
    {
        let old = *prev_encoder_state.borrow();
        let new_state = (props.share_screen, props.mic_enabled, props.video_enabled);

        if old != new_state {
            *prev_encoder_state.borrow_mut() = new_state;

            // Read encoders without subscribing (not in a reactive context)
            let enc_clone = encoders.peek().clone();
            let client_clone = props.client.clone();

            // Defer encoder operations to after the DOM update (so video element exists)
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(ref enc) = enc_clone {
                    let mut enc = enc.borrow_mut();

                    // Screen share transition
                    if new_state.0 != old.0 {
                        if new_state.0 {
                            enc.screen.set_enabled(true);
                            enc.screen.start();
                        } else {
                            enc.screen.stop();
                            enc.screen.set_enabled(false);
                        }
                    }

                    // Microphone transition
                    if new_state.1 != old.1 {
                        if new_state.1 {
                            enc.microphone.set_enabled(true);
                            enc.microphone.start();
                        } else {
                            enc.microphone.stop();
                            enc.microphone.set_enabled(false);
                        }
                    }

                    // Video transition
                    if new_state.2 != old.2 {
                        if new_state.2 {
                            enc.camera.set_enabled(true);
                            enc.camera.start();
                        } else {
                            enc.camera.stop();
                            enc.camera.set_enabled(false);
                        }
                    }
                }

                // Update client states
                if let Some(ref client) = client_clone {
                    client.set_audio_enabled(new_state.1);
                    client.set_video_enabled(new_state.2);
                }
            });
        }
    }

    // Device change handler factory functions
    let make_microphone_handler = || {
        move |device: DeviceInfo| {
            log::info!("Microphone changed: {:?}", device);
            if let Some(ref enc) = *encoders.read() {
                let mut enc = enc.borrow_mut();
                enc.media_devices.audio_inputs.select(&device.device_id);
                if enc.microphone.select(device.device_id.clone()) {
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
        }
    };

    let make_camera_handler = || {
        move |device: DeviceInfo| {
            log::info!("Camera changed: {:?}", device);
            if let Some(ref enc) = *encoders.read() {
                let mut enc = enc.borrow_mut();
                enc.media_devices.video_inputs.select(&device.device_id);
                if enc.camera.select(device.device_id.clone()) {
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
        }
    };

    let client_for_speaker_1 = client.clone();
    let client_for_speaker_2 = client.clone();

    let make_speaker_handler = |speaker_client: Option<VideoCallClient>| {
        move |device: DeviceInfo| {
            log::info!("Speaker changed: {:?}", device);
            if let Some(ref enc) = *encoders.read() {
                enc.borrow_mut()
                    .media_devices
                    .audio_outputs
                    .select(&device.device_id);
            }
            if let Some(ref client) = speaker_client {
                if let Err(e) = client.update_speaker_device(Some(device.device_id.clone())) {
                    log::error!("Failed to update speaker device: {e:?}");
                }
            }
            selected_speaker_id.set(device.device_id);
        }
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
                on_microphone_select: make_microphone_handler(),
                on_camera_select: make_camera_handler(),
                on_speaker_select: make_speaker_handler(client_for_speaker_1.clone())
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
            on_microphone_select: make_microphone_handler(),
            on_camera_select: make_camera_handler(),
            on_speaker_select: make_speaker_handler(client_for_speaker_2.clone()),
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
