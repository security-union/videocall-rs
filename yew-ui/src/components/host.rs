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
use futures::channel::mpsc;
use gloo_timers::callback::Timeout;
use log::debug;
use videocall_client::{create_microphone_encoder, MicrophoneEncoderTrait};
use videocall_client::{CameraEncoder, MediaDeviceList, ScreenEncoder, VideoCallClient};
use videocall_types::protos::media_packet::media_packet::MediaType;
use yew::prelude::*;

use crate::components::{
    device_selector::DeviceSelector, device_settings_modal::DeviceSettingsModal,
};
use crate::context::{is_valid_username, load_username_from_storage, save_username_to_storage};

const VIDEO_ELEMENT_ID: &str = "webcam";

#[derive(Debug)]
pub enum Msg {
    Start,
    EnableScreenShare,
    DisableScreenShare,
    EnableMicrophone(bool),
    DisableMicrophone,
    EnableVideo(bool),
    DisableVideo,
    AudioDeviceChanged(DeviceInfo),
    VideoDeviceChanged(DeviceInfo),
    SpeakerDeviceChanged(DeviceInfo),
    CameraEncoderSettingsUpdated(String),
    MicrophoneEncoderSettingsUpdated(String),
    ScreenEncoderSettingsUpdated(String),
    DevicesLoaded,
    DevicesChanged,
    ToggleChangeNameModal,
    UpdatePendingName(String),
    SaveChangeName,
    CancelChangeName,
    MicrophoneError(String),
}

pub struct Host {
    pub camera: CameraEncoder,
    pub microphone: Box<dyn MicrophoneEncoderTrait>,
    pub screen: ScreenEncoder,
    pub media_devices: MediaDeviceList,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub encoder_settings: EncoderSettings,
    show_change_name: bool,
    pending_name: String,
    change_name_error: Option<String>,
}

pub struct EncoderSettings {
    pub camera: Option<String>,
    pub microphone: Option<String>,
    pub screen: Option<String>,
}

/// Beautify the encoder settings for display.
/// Keep in mind that this should contain 1 line per encoder.
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

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,

    pub client: VideoCallClient,

    pub share_screen: bool,

    pub mic_enabled: bool,

    pub video_enabled: bool,

    pub on_encoder_settings_update: Callback<String>,

    pub device_settings_open: bool,

    pub on_device_settings_toggle: Callback<MouseEvent>,

    /// Called when the microphone encoder reports an unrecoverable error.
    /// The parent should disable the mic and optionally display an error.
    #[prop_or_default]
    pub on_microphone_error: Callback<String>,

    #[prop_or_default]
    pub register_toggle_change_name: Callback<Callback<MouseEvent>>,
}

impl Component for Host {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(ctx: &Context<Self>) -> Self {
        let client = ctx.props().client.clone();

        // Create 3 callbacks for the 3 encoders
        let camera_callback = ctx.link().callback(Msg::CameraEncoderSettingsUpdated);
        let microphone_callback = ctx.link().callback(Msg::MicrophoneEncoderSettingsUpdated);
        let screen_callback = ctx.link().callback(Msg::ScreenEncoderSettingsUpdated);

        let video_bitrate = video_bitrate_kbps().unwrap_or(1000);
        let mut camera = CameraEncoder::new(
            client.clone(),
            VIDEO_ELEMENT_ID,
            video_bitrate,
            camera_callback,
        );

        // Use the factory function to create the appropriate microphone encoder
        let audio_bitrate = audio_bitrate_kbps().unwrap_or(65);
        let microphone_error_cb = ctx.link().callback(Msg::MicrophoneError);
        let mut microphone = create_microphone_encoder(
            client.clone(),
            audio_bitrate,
            microphone_callback,
            microphone_error_cb.clone(),
        );

        let screen_bitrate = screen_bitrate_kbps().unwrap_or(1000);
        let mut screen = ScreenEncoder::new(client.clone(), screen_bitrate, screen_callback);

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

        // Set up callbacks for device list updates
        media_devices.on_loaded = {
            let link = ctx.link().clone();
            log::info!("Devices loaded 1");
            Callback::from(move |_| link.send_message(Msg::DevicesLoaded))
        };

        media_devices.on_devices_changed = {
            let link = ctx.link().clone();
            log::info!("Devices changed");
            Callback::from(move |_| link.send_message(Msg::DevicesChanged))
        };

        // Load devices
        media_devices.load();

        ctx.props()
            .register_toggle_change_name
            .emit(ctx.link().callback(|_: MouseEvent| Msg::ToggleChangeNameModal));

        Self {
            camera,
            microphone,
            screen,
            media_devices,
            share_screen: ctx.props().share_screen,
            mic_enabled: ctx.props().mic_enabled,
            video_enabled: ctx.props().video_enabled,
            encoder_settings: EncoderSettings {
                camera: None,
                microphone: None,
                screen: None,
            },
            show_change_name: false,
            pending_name: String::new(),
            change_name_error: None,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if self.screen.set_enabled(ctx.props().share_screen) && ctx.props().share_screen {
            self.share_screen = ctx.props().share_screen;
            let link = ctx.link().clone();
            let timeout = Timeout::new(1000, move || {
                link.send_message(Msg::EnableScreenShare);
            });
            timeout.forget();
        } else if self.share_screen != ctx.props().share_screen {
            self.share_screen = ctx.props().share_screen;
            ctx.link().send_message(Msg::DisableScreenShare);
        }
        // Mic enable/disable is controlled upstream; reflect props only
        if self.microphone.set_enabled(ctx.props().mic_enabled) {
            self.mic_enabled = ctx.props().mic_enabled;
            ctx.link()
                .send_message(Msg::EnableMicrophone(ctx.props().mic_enabled));
        } else if self.mic_enabled != ctx.props().mic_enabled {
            self.mic_enabled = ctx.props().mic_enabled;
            ctx.link().send_message(Msg::DisableMicrophone)
        }
        if self.camera.set_enabled(ctx.props().video_enabled) {
            self.video_enabled = ctx.props().video_enabled;
            ctx.link()
                .send_message(Msg::EnableVideo(ctx.props().video_enabled));
        } else if self.video_enabled != ctx.props().video_enabled {
            self.video_enabled = ctx.props().video_enabled;
            ctx.link().send_message(Msg::DisableVideo)
        }

        // Update videocallclient with the encoder settings
        // TODO: use atomic bools for the encoders
        ctx.props().client.set_audio_enabled(self.mic_enabled);
        ctx.props().client.set_video_enabled(self.video_enabled);
        ctx.props().client.set_screen_enabled(self.share_screen);

        if first_render {
            ctx.link().send_message(Msg::Start);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        log::debug!("Host update: {msg:?}");
        let should_update = match msg {
            Msg::EnableScreenShare => {
                self.screen.start();
                true
            }
            Msg::DisableScreenShare => {
                self.screen.stop();
                self.encoder_settings.screen = None;
                ctx.props()
                    .on_encoder_settings_update
                    .emit(self.encoder_settings.to_string());
                true
            }
            Msg::Start => true,
            Msg::EnableMicrophone(should_enable) => {
                if !should_enable {
                    return true;
                }
                self.microphone.start();
                true
            }
            Msg::DisableMicrophone => {
                self.microphone.stop();
                self.encoder_settings.microphone = None;
                ctx.props()
                    .on_encoder_settings_update
                    .emit(self.encoder_settings.to_string());
                true
            }
            Msg::MicrophoneError(err) => {
                log::error!("Microphone error: {err}");
                // Cancel encoder
                self.microphone.stop();
                // Propagate upstream so the parent can disable mic and show UI
                ctx.props().on_microphone_error.emit(err);
                true
            }
            Msg::EnableVideo(should_enable) => {
                if !should_enable {
                    return true;
                }
                self.camera.start();
                true
            }
            Msg::DisableVideo => {
                self.camera.stop();
                self.encoder_settings.camera = None;
                ctx.props()
                    .on_encoder_settings_update
                    .emit(self.encoder_settings.to_string());
                true
            }
            Msg::ToggleChangeNameModal => {
                if !self.show_change_name {
                    // Opening: prefill with current username from storage
                    self.pending_name = load_username_from_storage().unwrap_or_default();
                    self.show_change_name = true;
                    self.change_name_error = None;
                } else {
                    self.show_change_name = false;
                    self.change_name_error = None;
                }
                true
            }
            Msg::UpdatePendingName(value) => {
                self.pending_name = value;
                true
            }
            Msg::CancelChangeName => {
                self.show_change_name = false;
                self.change_name_error = None;
                false
            }
            Msg::SaveChangeName => {
                let new_name = self.pending_name.trim().to_string();
                if is_valid_username(&new_name) && !new_name.is_empty() {
                    save_username_to_storage(&new_name);
                    // Force a soft reload so meeting picks up new name
                    if let Some(win) = web_sys::window() {
                        let _ = win.location().reload();
                    }
                    self.show_change_name = false;
                    self.change_name_error = None;
                    false
                } else {
                    self.change_name_error =
                        Some("Use letters, numbers, and underscore only.".to_string());
                    true
                }
            }
            Msg::AudioDeviceChanged(audio) => {
                log::info!("Audio device changed: {audio}");
                // Update the MediaDeviceList selection
                self.media_devices.audio_inputs.select(&audio.device_id);
                if self.microphone.select(audio.device_id.clone()) {
                    let link = ctx.link().clone();
                    let timeout = Timeout::new(1000, move || {
                        link.send_message(Msg::EnableMicrophone(true));
                    });
                    timeout.forget();
                }
                true // Need to re-render to update device selector displays
            }
            Msg::VideoDeviceChanged(video) => {
                log::info!("Video device changed: {video}");
                // Update the MediaDeviceList selection
                self.media_devices.video_inputs.select(&video.device_id);
                if self.camera.select(video.device_id.clone()) {
                    let link = ctx.link().clone();
                    let timeout = Timeout::new(1000, move || {
                        link.send_message(Msg::EnableVideo(true));
                    });
                    timeout.forget();
                }
                true // Need to re-render to update device selector displays
            }
            Msg::SpeakerDeviceChanged(speaker) => {
                // Update the MediaDeviceList selection
                self.media_devices.audio_outputs.select(&speaker.device_id);
                // Update the speaker device for all connected peers
                if let Err(e) = ctx
                    .props()
                    .client
                    .update_speaker_device(Some(speaker.device_id.clone()))
                {
                    log::error!("Failed to update speaker device: {e:?}");
                }
                true
            }
            Msg::CameraEncoderSettingsUpdated(settings) => {
                // Only update if settings have changed
                if self.encoder_settings.camera.as_ref() != Some(&settings) {
                    self.encoder_settings.camera = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    true
                } else {
                    false
                }
            }
            Msg::MicrophoneEncoderSettingsUpdated(settings) => {
                // Only update if settings have changed
                if self.encoder_settings.microphone.as_ref() != Some(&settings) {
                    self.encoder_settings.microphone = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    true
                } else {
                    false
                }
            }
            Msg::ScreenEncoderSettingsUpdated(settings) => {
                // Only update if settings have changed
                if self.encoder_settings.screen.as_ref() != Some(&settings) {
                    self.encoder_settings.screen = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    true
                } else {
                    false
                }
            }
            Msg::DevicesLoaded => {
                let audio_device_id = self.media_devices.audio_inputs.selected();
                let video_device_id = self.media_devices.video_inputs.selected();
                let speaker_device_id = self.media_devices.audio_outputs.selected();

                ctx.link().send_message(Msg::AudioDeviceChanged(
                    self.create_device_info_from_id(&audio_device_id, "audio_input"),
                ));
                ctx.link().send_message(Msg::VideoDeviceChanged(
                    self.create_device_info_from_id(&video_device_id, "video_input"),
                ));
                ctx.link().send_message(Msg::SpeakerDeviceChanged(
                    self.create_device_info_from_id(&speaker_device_id, "audio_output"),
                ));
                true
            }
            Msg::DevicesChanged => {
                let audio_device_id = self.media_devices.audio_inputs.selected();
                let video_device_id = self.media_devices.video_inputs.selected();
                let speaker_device_id = self.media_devices.audio_outputs.selected();

                ctx.link().send_message(Msg::AudioDeviceChanged(
                    self.create_device_info_from_id(&audio_device_id, "audio_input"),
                ));
                ctx.link().send_message(Msg::VideoDeviceChanged(
                    self.create_device_info_from_id(&video_device_id, "video_input"),
                ));
                ctx.link().send_message(Msg::SpeakerDeviceChanged(
                    self.create_device_info_from_id(&speaker_device_id, "audio_output"),
                ));
                true
            }
        };
        log::debug!("Host update: {should_update:?}");
        should_update
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mic_callback = ctx.link().callback(Msg::AudioDeviceChanged);
        let cam_callback = ctx.link().callback(Msg::VideoDeviceChanged);
        let speaker_callback = ctx.link().callback(Msg::SpeakerDeviceChanged);
        let close_settings_callback = ctx.props().on_device_settings_toggle.clone();

        // Get device data from the centralized MediaDeviceList
        let microphones = self.media_devices.audio_inputs.devices();
        let cameras = self.media_devices.video_inputs.devices();
        let speakers = self.media_devices.audio_outputs.devices();

        let selected_microphone_id = self.media_devices.audio_inputs.selected();
        let selected_camera_id = self.media_devices.video_inputs.selected();
        let selected_speaker_id = self.media_devices.audio_outputs.selected();

        html! {
            <>
                {
                    if ctx.props().video_enabled {
                        html! {
                            <div class="host-video-wrapper" style="position:relative;">
                                // <video class="self-camera" autoplay=true id={VIDEO_ELEMENT_ID} playsinline={true} controls={false}></video>
                                <button class="change-name-fab" title="Change name"
                                    onclick={ctx.link().callback(|_| Msg::ToggleChangeNameModal)}>
                                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>
                                </button>
                            </div>
                        }
                    } else {
                        html! {
                            <div class="" style="padding:1rem; display:flex; align-items:center; justify-content:center; border-radius: 1rem; position:relative;">
                                <div class="placeholder-content">
                                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                        <path d="M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10"></path>
                                        <line x1="1" y1="1" x2="23" y2="23"></line>
                                    </svg>
                                    <span class="placeholder-text">{"Camera Off"}</span>
                                </div>
                                <button class="change-name-fab" title="Change name"
                                    onclick={ctx.link().callback(|_| Msg::ToggleChangeNameModal)}>
                                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>
                                </button>
                            </div>
                        }
                    }
                }

                // Device Settings Menu Button (positioned outside the host video)
                <button
                    class="device-settings-menu-button btn-apple btn-secondary"
                    onclick={ctx.props().on_device_settings_toggle.clone()}
                    title="Device Settings"
                >
                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <circle cx="12" cy="12" r="3"></circle>
                        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
                    </svg>
                </button>

                // Desktop Device Selector (hidden on mobile)
                <div class="desktop-device-selector">
                    <DeviceSelector
                        microphones={microphones.clone()}
                        cameras={cameras.clone()}
                        speakers={speakers.clone()}
                        selected_microphone_id={selected_microphone_id.clone()}
                        selected_camera_id={selected_camera_id.clone()}
                        selected_speaker_id={selected_speaker_id.clone()}
                        on_microphone_select={mic_callback.clone()}
                        on_camera_select={cam_callback.clone()}
                        on_speaker_select={speaker_callback.clone()}
                    />
                </div>

                // Mobile Device Settings Modal
                <DeviceSettingsModal
                    microphones={microphones}
                    cameras={cameras}
                    speakers={speakers}
                    selected_microphone_id={selected_microphone_id.clone()}
                    selected_camera_id={selected_camera_id.clone()}
                    selected_speaker_id={selected_speaker_id.clone()}
                    on_microphone_select={mic_callback}
                    on_camera_select={cam_callback}
                    on_speaker_select={speaker_callback}
                    visible={ctx.props().device_settings_open}
                    on_close={close_settings_callback}
                />

                {
                    if self.show_change_name {
                        html!{
                            <div class="glass-backdrop"
                                onkeydown={{
                                    let link = ctx.link().clone();
                                    Callback::from(move |e: KeyboardEvent| {
                                        let key = e.key();
                                        if key == "Escape" {
                                            e.prevent_default();
                                            link.send_message(Msg::CancelChangeName);
                                        } else if key == "Enter" {
                                            e.prevent_default();
                                            link.send_message(Msg::SaveChangeName);
                                        }
                                    })
                                }}
                            >
                                <div class="card-apple" style="width: 380px;">
                                    <h3 style="margin-top:0;">{"Change your name"}</h3>
                                    <p style="color:#AEAEB2; margin-top:0.25rem;">{"This name will be visible to others in the meeting."}</p>
                                    <input class="input-apple" value={self.pending_name.clone()} oninput={ctx.link().callback(|e: InputEvent| {
                                        let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                                        Msg::UpdatePendingName(input.value())
                                    })} placeholder="Enter new name" pattern="^[a-zA-Z0-9_]*$" autofocus=true
                                        onkeydown={{
                                            let link = ctx.link().clone();
                                            Callback::from(move |e: KeyboardEvent| {
                                                let key = e.key();
                                                if key == "Escape" {
                                                    e.prevent_default();
                                                    link.send_message(Msg::CancelChangeName);
                                                } else if key == "Enter" {
                                                    e.prevent_default();
                                                    link.send_message(Msg::SaveChangeName);
                                                }
                                            })
                                        }}
                                    />
                                    { if let Some(err) = &self.change_name_error { html!{ <p style="color:#FF453A; margin-top:6px; font-size:12px;">{err}</p> } } else { html!{} } }
                                    <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:12px;">
                                        <button class="btn-apple btn-secondary btn-sm" onclick={ctx.link().callback(|_| Msg::CancelChangeName)}>{"Cancel"}</button>
                                        <button class="btn-apple btn-primary btn-sm" onclick={ctx.link().callback(|_| Msg::SaveChangeName)}>{"Save"}</button>
                                    </div>
                                </div>
                            </div>
                        }
                    } else { html!{} }
                }
            </>
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        debug!("destroying");
        self.camera.stop();
        self.microphone.stop();
        self.screen.stop();
    }
}

impl Host {
    /// Helper method to create DeviceInfo from device ID by looking up the device name
    fn create_device_info_from_id(&self, device_id: &str, device_type: &str) -> DeviceInfo {
        let device_name = match device_type {
            "audio_input" => self
                .media_devices
                .audio_inputs
                .devices()
                .iter()
                .find(|device| device.device_id() == device_id)
                .map(|device| device.label())
                .unwrap_or_else(|| "Unknown Microphone".to_string()),
            "video_input" => self
                .media_devices
                .video_inputs
                .devices()
                .iter()
                .find(|device| device.device_id() == device_id)
                .map(|device| device.label())
                .unwrap_or_else(|| "Unknown Camera".to_string()),
            "audio_output" => self
                .media_devices
                .audio_outputs
                .devices()
                .iter()
                .find(|device| device.device_id() == device_id)
                .map(|device| device.label())
                .unwrap_or_else(|| "Unknown Speaker".to_string()),
            _ => "Unknown Device".to_string(),
        };

        DeviceInfo::new(device_id.to_string(), device_name)
    }
}
