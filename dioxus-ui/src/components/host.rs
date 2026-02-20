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

use crate::constants::*;
use crate::types::DeviceInfo;
use dioxus::prelude::*;
use futures::channel::mpsc;
use gloo_timers::callback::Timeout;
use videocall_client::Callback as VcCallback;
use videocall_client::{create_microphone_encoder, MicrophoneEncoderTrait};
use videocall_client::{CameraEncoder, MediaDeviceList, ScreenEncoder, ScreenShareEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;

use crate::components::{
    device_selector::DeviceSelector, device_settings_modal::DeviceSettingsModal,
};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, VideoCallClientCtx,
};

use std::cell::RefCell;
use std::rc::Rc;

const VIDEO_ELEMENT_ID: &str = "webcam";

struct EncoderSettings {
    camera: Option<String>,
    microphone: Option<String>,
    screen: Option<String>,
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

#[component]
pub fn Host(
    share_screen: bool,
    mic_enabled: bool,
    video_enabled: bool,
    on_encoder_settings_update: EventHandler<String>,
    device_settings_open: bool,
    on_device_settings_toggle: EventHandler<MouseEvent>,
    #[props(default)] on_microphone_error: EventHandler<String>,
    #[props(default)] on_camera_error: EventHandler<String>,
    on_screen_share_state: EventHandler<ScreenShareEvent>,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    // Indirection cells for callbacks: updated each render, closed over by encoder callbacks
    let camera_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let mic_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let screen_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let camera_error_handler: Rc<RefCell<Option<EventHandler<String>>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let mic_error_handler: Rc<RefCell<Option<EventHandler<String>>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let screen_state_handler: Rc<RefCell<Option<EventHandler<ScreenShareEvent>>>> = use_hook(|| Rc::new(RefCell::new(None)));

    // Use Rc<RefCell<>> to hold mutable encoder state that persists across renders
    let state = use_hook(|| {
        let video_bitrate = video_bitrate_kbps().unwrap_or(1000);
        let audio_bitrate = audio_bitrate_kbps().unwrap_or(65);
        let screen_bitrate = screen_bitrate_kbps().unwrap_or(1000);

        let cam_settings_cell = camera_settings_handler.clone();
        let camera_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = cam_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let cam_error_cell = camera_error_handler.clone();
        let camera_error_cb = VcCallback::from(move |err: String| {
            if let Some(handler) = cam_error_cell.borrow().as_ref() {
                handler.call(err);
            }
        });
        let mut camera = CameraEncoder::new(
            client.clone(),
            VIDEO_ELEMENT_ID,
            video_bitrate,
            camera_settings_cb,
            camera_error_cb,
        );

        let mic_settings_cell = mic_settings_handler.clone();
        let mic_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = mic_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let mic_error_cell = mic_error_handler.clone();
        let mic_error_cb = VcCallback::from(move |err: String| {
            if let Some(handler) = mic_error_cell.borrow().as_ref() {
                handler.call(err);
            }
        });
        let mut microphone = create_microphone_encoder(
            client.clone(),
            audio_bitrate,
            mic_settings_cb,
            mic_error_cb,
        );

        let screen_settings_cell = screen_settings_handler.clone();
        let screen_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = screen_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let screen_state_cell = screen_state_handler.clone();
        let screen_state_cb = VcCallback::from(move |event: ScreenShareEvent| {
            if let Some(handler) = screen_state_cell.borrow().as_ref() {
                handler.call(event);
            }
        });
        let mut screen = ScreenEncoder::new(
            client.clone(),
            screen_bitrate,
            screen_settings_cb,
            screen_state_cb,
        );

        // Wire up encoder controls
        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::VIDEO);
        camera.set_encoder_control(rx);

        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::AUDIO);
        microphone.set_encoder_control(rx);

        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::SCREEN);
        screen.set_encoder_control(rx);

        // Create MediaDeviceList
        let media_devices = MediaDeviceList::new();

        Rc::new(RefCell::new(HostState {
            camera,
            microphone,
            screen,
            media_devices,
            encoder_settings: EncoderSettings {
                camera: None,
                microphone: None,
                screen: None,
            },
            prev_share_screen: false,
            prev_mic_enabled: false,
            prev_video_enabled: false,
            initialized: false,
        }))
    });

    // Update the indirection cells so encoder callbacks route to the current EventHandlers.
    // This runs on every render to keep them in sync with the latest prop values.
    *camera_settings_handler.borrow_mut() = Some(on_encoder_settings_update.clone());
    *mic_settings_handler.borrow_mut() = Some(on_encoder_settings_update.clone());
    *screen_settings_handler.borrow_mut() = Some(on_encoder_settings_update.clone());
    *camera_error_handler.borrow_mut() = Some(on_camera_error.clone());
    *mic_error_handler.borrow_mut() = Some(on_microphone_error.clone());
    *screen_state_handler.borrow_mut() = Some(on_screen_share_state.clone());

    // Initialize devices once
    {
        let state = state.clone();
        use_effect(move || {
            let state_for_loaded = state.clone();
            let mut s = state.borrow_mut();
            if !s.initialized {
                s.media_devices.on_loaded = VcCallback::from(move |_| {
                    let mut s = state_for_loaded.borrow_mut();
                    let video_id = s.media_devices.video_inputs.selected();
                    let audio_id = s.media_devices.audio_inputs.selected();
                    let num_cameras = s.media_devices.video_inputs.devices().len();
                    let num_mics = s.media_devices.audio_inputs.devices().len();

                    log::info!(
                        "Host on_loaded: cameras={num_cameras} mics={num_mics} \
                         video_id='{video_id}' audio_id='{audio_id}' \
                         prev_video={} prev_mic={}",
                        s.prev_video_enabled,
                        s.prev_mic_enabled,
                    );

                    // Auto-select camera device
                    let cam_needs_start = if !video_id.is_empty() {
                        s.media_devices.video_inputs.select(&video_id);
                        // stop() clears both enabled and switching flags so that
                        // select() below does not set the switching flag (which
                        // would cause the new encoding loop to exit immediately).
                        let was_enabled = s.prev_video_enabled;
                        if was_enabled {
                            s.camera.stop();
                        }
                        s.camera.select(video_id);
                        if was_enabled {
                            s.camera.set_enabled(true);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    // Auto-select microphone device
                    let mic_needs_start = if !audio_id.is_empty() {
                        s.media_devices.audio_inputs.select(&audio_id);
                        let was_enabled = s.prev_mic_enabled;
                        if was_enabled {
                            s.microphone.stop();
                        }
                        s.microphone.select(audio_id);
                        if was_enabled {
                            s.microphone.set_enabled(true);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    log::info!(
                        "Host on_loaded: cam_needs_start={cam_needs_start} mic_needs_start={mic_needs_start}"
                    );

                    drop(s);

                    // Start encoders that were already enabled (camera/mic were
                    // toggled on before devices finished loading).
                    if cam_needs_start {
                        let sc = state_for_loaded.clone();
                        Timeout::new(500, move || {
                            log::info!("Host on_loaded: starting camera after timeout");
                            sc.borrow_mut().camera.start();
                        })
                        .forget();
                    }
                    if mic_needs_start {
                        let sc = state_for_loaded.clone();
                        Timeout::new(500, move || {
                            log::info!("Host on_loaded: starting microphone after timeout");
                            sc.borrow_mut().microphone.start();
                        })
                        .forget();
                    }
                });
                s.media_devices.on_devices_changed = VcCallback::noop();
                s.media_devices.load();
                s.initialized = true;
            }
        });
    }

    // Handle prop changes for screen/mic/video enables.
    // NOTE: This runs in the component body (not use_effect) because Dioxus 0.7
    // use_effect does NOT re-run when ReadOnlySignal props change.  The component
    // function itself re-runs whenever the parent passes new prop values.
    {
        let mut s = state.borrow_mut();

        log::info!(
            "Host render: video={video_enabled} prev={} mic={mic_enabled} prev={} screen={share_screen} prev={}",
            s.prev_video_enabled, s.prev_mic_enabled, s.prev_share_screen,
        );

        // Screen share
        if s.prev_share_screen != share_screen {
            s.prev_share_screen = share_screen;
            if share_screen {
                s.screen.set_enabled(true);
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().screen.start();
                }).forget();
            } else {
                s.screen.set_enabled(false);
                s.screen.stop();
                s.encoder_settings.screen = None;
            }
        }

        // Microphone
        if s.prev_mic_enabled != mic_enabled {
            s.prev_mic_enabled = mic_enabled;
            if mic_enabled {
                let device_id = s.media_devices.audio_inputs.selected();
                if !device_id.is_empty() {
                    s.microphone.select(device_id);
                }
                s.microphone.set_enabled(true);
                s.microphone.start();
            } else {
                s.microphone.set_enabled(false);
                s.microphone.stop();
                s.encoder_settings.microphone = None;
            }
        }

        // Camera
        if s.prev_video_enabled != video_enabled {
            s.prev_video_enabled = video_enabled;
            if video_enabled {
                let device_id = s.media_devices.video_inputs.selected();
                log::info!(
                    "Host render: camera ON, auto-select device_id='{device_id}'"
                );
                if !device_id.is_empty() {
                    s.camera.select(device_id);
                }
                s.camera.set_enabled(true);
                s.camera.start();
            } else {
                log::info!("Host render: camera OFF");
                s.camera.set_enabled(false);
                s.camera.stop();
                s.encoder_settings.camera = None;
            }
        }

        // Update client flags
        client.set_audio_enabled(mic_enabled);
        client.set_video_enabled(video_enabled);
        drop(s);
    }

    // Diagnostic: periodically log the state of the video element so we can
    // see whether srcObject is set, whether the element is in the DOM, etc.
    {
        use_effect(move || {
            spawn(async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(3_000).await;
                    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                        if let Some(elem) = doc.get_element_by_id(VIDEO_ELEMENT_ID) {
                            use wasm_bindgen::JsCast;
                            if let Ok(v) = elem.dyn_into::<web_sys::HtmlVideoElement>() {
                                let has_src = v.src_object().is_some();
                                let ready = v.ready_state();
                                let paused = v.paused();
                                let w = v.video_width();
                                let h = v.video_height();
                                let display = v.style().get_property_value("display").unwrap_or_default();
                                let parent_display = v.parent_element()
                                    .map(|p| p.get_attribute("style").unwrap_or_default())
                                    .unwrap_or_default();
                                log::info!(
                                    "DIAG video#webcam: srcObject={has_src} readyState={ready} \
                                     paused={paused} size={w}x{h} display='{display}' \
                                     parent_style='{parent_display}'"
                                );
                            }
                        } else {
                            log::warn!("DIAG video#webcam: NOT FOUND IN DOM");
                        }
                    }
                }
            });
        });
    }

    // Device change handlers (Rc-wrapped so they can be shared between two components)
    let on_mic_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        Rc::new(move |audio: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.audio_inputs.select(&audio.device_id);
            if s.microphone.select(audio.device_id.clone()) {
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().microphone.start();
                }).forget();
            }
        })
    };

    let on_cam_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        Rc::new(move |video: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.video_inputs.select(&video.device_id);
            if s.camera.select(video.device_id.clone()) {
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().camera.start();
                }).forget();
            }
        })
    };

    let on_speaker_change: Rc<dyn Fn(DeviceInfo)> = {
        let state = state.clone();
        let client = client.clone();
        Rc::new(move |speaker: DeviceInfo| {
            let mut s = state.borrow_mut();
            s.media_devices.audio_outputs.select(&speaker.device_id);
            if let Err(e) = client.update_speaker_device(Some(speaker.device_id.clone())) {
                log::error!("Failed to update speaker device: {e:?}");
            }
        })
    };

    // Change name state
    let mut show_change_name = use_signal(|| false);
    let mut pending_name = use_signal(String::new);
    let mut change_name_error = use_signal(|| None::<String>);

    // Get device data
    let s = state.borrow();
    let microphones = s.media_devices.audio_inputs.devices();
    let cameras = s.media_devices.video_inputs.devices();
    let speakers = s.media_devices.audio_outputs.devices();
    let selected_microphone_id = s.media_devices.audio_inputs.selected();
    let selected_camera_id = s.media_devices.video_inputs.selected();
    let selected_speaker_id = s.media_devices.audio_outputs.selected();
    drop(s);

    rsx! {
        // Always render the <video> element so Dioxus never destroys it.
        // The camera encoder attaches srcObject via JS; if Dioxus recreates
        // the element on re-render the stream reference is lost (dark square).
        // Dioxus patches individual CSS properties (doesn't replace the whole
        // style attribute), so both branches must set ALL properties explicitly.
        div {
            class: "host-video-wrapper",
            style: if video_enabled {
                "position:relative; width:auto; height:auto; opacity:1; overflow:visible; pointer-events:auto;"
            } else {
                "position:absolute; width:1px; height:1px; opacity:0; overflow:hidden; pointer-events:none;"
            },
            video { class: "self-camera", autoplay: true, id: VIDEO_ELEMENT_ID, playsinline: "true", muted: true, controls: false }
            button {
                class: "change-name-fab",
                title: "Change name",
                onclick: move |_| {
                    pending_name.set(load_username_from_storage().unwrap_or_default());
                    show_change_name.set(true);
                    change_name_error.set(None);
                },
                svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    path { d: "M12 20h9" }
                    path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z" }
                }
            }
        }
        if !video_enabled {
            div { style: "padding:1rem; display:flex; align-items:center; justify-content:center; border-radius: 1rem; position:relative;",
                div { class: "placeholder-content",
                    svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    }
                    span { class: "placeholder-text", "Camera Off" }
                }
                button {
                    class: "change-name-fab",
                    title: "Change name",
                    onclick: move |_| {
                        pending_name.set(load_username_from_storage().unwrap_or_default());
                        show_change_name.set(true);
                        change_name_error.set(None);
                    },
                    svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M12 20h9" }
                        path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z" }
                    }
                }
            }
        }

        // Device Settings Menu Button
        button {
            class: "device-settings-menu-button btn-apple btn-secondary",
            onclick: move |e| on_device_settings_toggle.call(e),
            title: "Device Settings",
            svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "3" }
                path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
            }
        }

        // Desktop Device Selector
        {
            let on_mic = on_mic_change.clone();
            let on_cam = on_cam_change.clone();
            let on_spk = on_speaker_change.clone();
            rsx! {
                div { class: "desktop-device-selector",
                    DeviceSelector {
                        microphones: microphones.clone(),
                        cameras: cameras.clone(),
                        speakers: speakers.clone(),
                        selected_microphone_id: selected_microphone_id.clone(),
                        selected_camera_id: selected_camera_id.clone(),
                        selected_speaker_id: selected_speaker_id.clone(),
                        on_microphone_select: move |d: DeviceInfo| on_mic(d),
                        on_camera_select: move |d: DeviceInfo| on_cam(d),
                        on_speaker_select: move |d: DeviceInfo| on_spk(d),
                    }
                }
            }
        }

        // Mobile Device Settings Modal
        {
            let on_mic = on_mic_change.clone();
            let on_cam = on_cam_change.clone();
            let on_spk = on_speaker_change.clone();
            rsx! {
                DeviceSettingsModal {
                    microphones: microphones,
                    cameras: cameras,
                    speakers: speakers,
                    selected_microphone_id: selected_microphone_id,
                    selected_camera_id: selected_camera_id,
                    selected_speaker_id: selected_speaker_id,
                    on_microphone_select: move |d: DeviceInfo| on_mic(d),
                    on_camera_select: move |d: DeviceInfo| on_cam(d),
                    on_speaker_select: move |d: DeviceInfo| on_spk(d),
                    visible: device_settings_open,
                    on_close: move |e| on_device_settings_toggle.call(e),
                }
            }
        }

        // Change Name Modal
        if show_change_name() {
            div {
                class: "glass-backdrop",
                onkeydown: move |e: Event<KeyboardData>| {
                    let key = e.key();
                    if key == Key::Escape {
                        show_change_name.set(false);
                        change_name_error.set(None);
                    } else if key == Key::Enter {
                        let new_name = pending_name().trim().to_string();
                        if is_valid_username(&new_name) && !new_name.is_empty() {
                            save_username_to_storage(&new_name);
                            if let Some(win) = web_sys::window() {
                                let _ = win.location().reload();
                            }
                        } else {
                            change_name_error.set(Some("Use letters, numbers, and underscore only.".to_string()));
                        }
                    }
                },
                div { class: "card-apple", style: "width: 380px;",
                    h3 { style: "margin-top:0;", "Change your name" }
                    p { style: "color:#AEAEB2; margin-top:0.25rem;", "This name will be visible to others in the meeting." }
                    input {
                        class: "input-apple",
                        value: "{pending_name}",
                        oninput: move |e: Event<FormData>| {
                            pending_name.set(e.value());
                        },
                        placeholder: "Enter new name",
                        pattern: "^[a-zA-Z0-9_]*$",
                        autofocus: true,
                    }
                    if let Some(err) = change_name_error() {
                        p { style: "color:#FF453A; margin-top:6px; font-size:12px;", "{err}" }
                    }
                    div { style: "display:flex; gap:8px; justify-content:flex-end; margin-top:12px;",
                        button {
                            class: "btn-apple btn-secondary btn-sm",
                            onclick: move |_| {
                                show_change_name.set(false);
                                change_name_error.set(None);
                            },
                            "Cancel"
                        }
                        button {
                            class: "btn-apple btn-primary btn-sm",
                            onclick: move |_| {
                                let new_name = pending_name().trim().to_string();
                                if is_valid_username(&new_name) && !new_name.is_empty() {
                                    save_username_to_storage(&new_name);
                                    if let Some(win) = web_sys::window() {
                                        let _ = win.location().reload();
                                    }
                                } else {
                                    change_name_error.set(Some("Use letters, numbers, and underscore only.".to_string()));
                                }
                            },
                            "Save"
                        }
                    }
                }
            }
        }
    }
}

struct HostState {
    camera: CameraEncoder,
    microphone: Box<dyn MicrophoneEncoderTrait>,
    screen: ScreenEncoder,
    media_devices: MediaDeviceList,
    encoder_settings: EncoderSettings,
    prev_share_screen: bool,
    prev_mic_enabled: bool,
    prev_video_enabled: bool,
    initialized: bool,
}
