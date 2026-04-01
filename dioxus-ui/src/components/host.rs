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
    canvas_generator::speak_style, device_settings_modal::DeviceSettingsModal,
};
use crate::context::VideoCallClientCtx;

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStream;

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
    #[props(default)] audio_level: f32,
    on_encoder_settings_update: EventHandler<String>,
    device_settings_open: bool,
    on_device_settings_toggle: EventHandler<()>,
    #[props(default)] on_microphone_error: EventHandler<String>,
    #[props(default)] on_camera_error: EventHandler<String>,
    on_screen_share_state: EventHandler<ScreenShareEvent>,
    reload_devices_counter: u32,
) -> Element {
    let client = use_context::<VideoCallClientCtx>();

    // Indirection cells for callbacks: updated each render, closed over by encoder callbacks
    let camera_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let mic_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let screen_settings_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let camera_error_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let mic_error_handler: Rc<RefCell<Option<EventHandler<String>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let screen_state_handler: Rc<RefCell<Option<EventHandler<ScreenShareEvent>>>> =
        use_hook(|| Rc::new(RefCell::new(None)));

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
        // Microphone encoder is created after camera so it can share the
        // camera's audio tier atomics (avoiding a duplicate quality manager).
        let microphone = create_microphone_encoder(
            client.clone(),
            audio_bitrate,
            mic_settings_cb,
            mic_error_cb,
            vad_threshold().ok(),
            Some(camera.shared_audio_tier_bitrate()),
            Some(camera.shared_audio_tier_fec()),
        );

        let screen_settings_cell = screen_settings_handler.clone();
        let screen_settings_cb = VcCallback::from(move |settings: String| {
            if let Some(handler) = screen_settings_cell.borrow().as_ref() {
                handler.call(settings);
            }
        });
        let screen_state_cell = screen_state_handler.clone();
        let screen_state_cb = VcCallback::from(move |event: ScreenShareEvent| {
            match &event {
                ScreenShareEvent::Started(stream) => {
                    attach_screen_preview(stream);
                }
                _ => {
                    detach_screen_preview();
                }
            }
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

        // Wire up congestion step-down and PLI keyframe flags
        camera.set_congestion_step_down_flag(client.congestion_step_down_flag());
        camera.set_force_keyframe_flag(client.force_camera_keyframe_flag());
        screen.set_force_keyframe_flag(client.force_screen_keyframe_flag());

        // Wire up encoder controls. The microphone encoder no longer needs
        // its own diagnostics channel — it reads audio tier settings from
        // the camera encoder's shared atomics.
        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::VIDEO);
        camera.set_encoder_control(rx);

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
            last_reload_counter: 0,
        }))
    });

    // Update the indirection cells so encoder callbacks route to the current EventHandlers.
    // This runs on every render to keep them in sync with the latest prop values.
    *camera_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *mic_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *screen_settings_handler.borrow_mut() = Some(on_encoder_settings_update);
    *camera_error_handler.borrow_mut() = Some(on_camera_error);
    *mic_error_handler.borrow_mut() = Some(on_microphone_error);
    *screen_state_handler.borrow_mut() = Some(on_screen_share_state);

    // Initialize devices once
    {
        let state = state.clone();
        let value = client.clone();
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
                let state_for_devices_changed = state.clone();
                let client_for_devices_changed = value.clone();
                s.media_devices.on_devices_changed = VcCallback::from(move |_| {
                    let mut s = state_for_devices_changed.borrow_mut();

                    let audio_device_id = s.media_devices.audio_inputs.selected();
                    let video_device_id = s.media_devices.video_inputs.selected();
                    let speaker_device_id = s.media_devices.audio_outputs.selected();

                    let mut mic_needs_start = false;
                    if !audio_device_id.is_empty() {
                        s.media_devices.audio_inputs.select(&audio_device_id);
                        mic_needs_start = s.microphone.select(audio_device_id);
                    }

                    let mut cam_needs_start = false;
                    if !video_device_id.is_empty() {
                        s.media_devices.video_inputs.select(&video_device_id);
                        cam_needs_start = s.camera.select(video_device_id);
                    }

                    if !speaker_device_id.is_empty() {
                        s.media_devices.audio_outputs.select(&speaker_device_id);
                        if let Err(e) = client_for_devices_changed
                            .update_speaker_device(Some(speaker_device_id))
                        {
                            log::error!("Failed to update speaker device: {e:?}");
                        }
                    }

                    drop(s);

                    if mic_needs_start {
                        let sc = state_for_devices_changed.clone();
                        Timeout::new(1000, move || {
                            sc.borrow_mut().microphone.start();
                        })
                        .forget();
                    }
                    if cam_needs_start {
                        let sc = state_for_devices_changed.clone();
                        Timeout::new(1000, move || {
                            sc.borrow_mut().camera.start();
                        })
                        .forget();
                    }
                });
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

        if s.last_reload_counter != reload_devices_counter {
            s.media_devices.load();
            s.last_reload_counter = reload_devices_counter;
        }

        log::info!(
            "Host render: video={video_enabled} prev={} mic={mic_enabled} prev={} screen={share_screen} prev={}",
            s.prev_video_enabled, s.prev_mic_enabled, s.prev_share_screen,
        );

        // Screen share
        if s.prev_share_screen != share_screen {
            s.prev_share_screen = share_screen;
            if share_screen {
                s.screen.set_enabled(true);
                log::info!("Start screen share encoder");
                let state_clone = state.clone();
                Timeout::new(1000, move || {
                    state_clone.borrow_mut().screen.start();
                })
                .forget();
            } else {
                s.screen.set_enabled(false);
                s.screen.stop();
                detach_screen_preview();
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
                log::info!("Host render: camera ON, auto-select device_id='{device_id}'");
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

    // Periodically re-affirm audio/video enabled flags on the active connection.
    //
    // In Yew, the Host component re-renders ~every second (driven by encoder
    // settings update messages), and rendered() calls set_video_enabled /
    // set_audio_enabled each time.  In Dioxus, the component body only runs
    // when props change, so after the initial enable the flags are never
    // re-affirmed.  If the underlying connection is lost and re-established,
    // the new Connection object starts with video_enabled=false.  Without
    // periodic re-affirmation the heartbeat keeps reporting video_enabled=false
    // to the server, causing peers to see the video toggling off.
    {
        let state = state.clone();
        let client = client.clone();
        use_effect(move || {
            let state = state.clone();
            let client = client.clone();
            spawn(async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                    let s = state.borrow();
                    client.set_audio_enabled(s.prev_mic_enabled);
                    client.set_video_enabled(s.prev_video_enabled);
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
                })
                .forget();
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
                })
                .forget();
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

    // Get device data
    let s = state.borrow();
    let microphones = s.media_devices.audio_inputs.devices();
    let cameras = s.media_devices.video_inputs.devices();
    let speakers = s.media_devices.audio_outputs.devices();
    let selected_microphone_id = s.media_devices.audio_inputs.selected();
    let selected_camera_id = s.media_devices.video_inputs.selected();
    let selected_speaker_id = s.media_devices.audio_outputs.selected();
    drop(s);

    let glow = speak_style(audio_level);

    rsx! {
        // Always render the <video> element so Dioxus never destroys it.
        // The camera encoder attaches srcObject via JS; if Dioxus recreates
        // the element on re-render the stream reference is lost (dark square).
        // Dioxus patches individual CSS properties (doesn't replace the whole
        // style attribute), so both branches must set ALL properties explicitly.
        div {
            class: "host-video-wrapper",
            style: if video_enabled {
                "position:relative; width:100%; height:auto; opacity:1; overflow:hidden; pointer-events:auto;"
            } else {
                "position:absolute; width:1px; height:1px; opacity:0; overflow:hidden; pointer-events:none;"
            },
            video { class: "self-camera", autoplay: true, id: VIDEO_ELEMENT_ID, playsinline: "true", muted: true, controls: false }
            // Glow overlay renders ON TOP of the video element
            if video_enabled {
                div {
                    style: "{glow}",
                    class: "glow-overlay",
                }
            }
        }
        // Always-mounted screen share preview — toggled via style so the element
        // exists in the DOM before attach_screen_preview() runs.
        // Positioned AFTER the camera so the preview appears below it.
        video {
            id: "screen-share-preview",
            class: "screen-share-preview",
            style: if share_screen { "display:block;" } else { "display:none;" },
            autoplay: true,
            muted: true,
            playsinline: "true",
            controls: false,
        }
        if !video_enabled {
            div {
                style: "padding:1rem; display:flex; align-items:center; justify-content:center; border-radius: 0; position:relative; border: 1.5px solid transparent; width:100%; aspect-ratio:16/9;",
                div { class: "placeholder-content",
                    svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    }
                    span { class: "placeholder-text", "Camera Off" }
                }
                // Glow overlay renders ON TOP of content
                div {
                    style: "{glow}",
                    class: "glow-overlay",
                }
            }
        }

        // Device Settings Menu Button
        button {
            class: "device-settings-menu-button btn-apple btn-secondary",
            onclick: move |_| on_device_settings_toggle.call(()),
            title: "Device Settings",
            svg { xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "3" }
                 path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
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
                    on_close: move |_| on_device_settings_toggle.call(())
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
    last_reload_counter: u32,
}

fn attach_screen_preview(stream: &MediaStream) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("screen-share-preview"))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        // Explicitly set the muted property (not just the HTML attribute) so that
        // Chrome's autoplay policy recognises the element as muted and allows play().
        video.set_muted(true);
        video.set_src_object(Some(stream));
        // Properly await the play() Promise via spawn_local.
        // Dropping the Promise with `let _` causes Chrome to silently abort
        // playback for display-capture streams; Edge is more lenient.
        wasm_bindgen_futures::spawn_local(async move {
            match video.play() {
                Ok(promise) => {
                    if let Err(e) = JsFuture::from(promise).await {
                        log::warn!("Screen preview play() rejected: {:?}", e);
                    }
                }
                Err(e) => log::warn!("Screen preview play() error: {:?}", e),
            }
        });
    }
}

fn detach_screen_preview() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("screen-share-preview"))
    {
        let video: web_sys::HtmlVideoElement = el.unchecked_into();
        video.set_src_object(None);
    }
}
