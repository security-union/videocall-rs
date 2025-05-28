use crate::constants::*;
use futures::channel::mpsc;
use gloo_timers::callback::Timeout;
use log::debug;
use std::fmt;
use videocall_client::{create_microphone_encoder, MicrophoneEncoderTrait};
use videocall_client::{CameraEncoder, ScreenEncoder, VideoCallClient};
use videocall_types::protos::media_packet::media_packet::MediaType;
use web_sys::window;
use yew::prelude::*;

use crate::components::device_selector::DeviceSelector;

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
    AudioDeviceChanged(String),
    VideoDeviceChanged(String),
    SpeakerDeviceChanged(String),
    CameraEncoderSettingsUpdated(String),
    MicrophoneEncoderSettingsUpdated(String),
    ScreenEncoderSettingsUpdated(String),
}

pub struct Host {
    pub camera: CameraEncoder,
    pub microphone: Box<dyn MicrophoneEncoderTrait>,
    pub screen: ScreenEncoder,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub encoder_settings: EncoderSettings,
    pub selected_speaker_id: Option<String>,
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
        write!(
            f,
            "Camera: {}\nMic: {}\nScreen: {}",
            self.camera.clone().unwrap_or("None".to_string()),
            self.microphone.clone().unwrap_or("None".to_string()),
            self.screen.clone().unwrap_or("None".to_string())
        )
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

        let mut camera = CameraEncoder::new(
            client.clone(),
            VIDEO_ELEMENT_ID,
            VIDEO_BITRATE_KBPS,
            camera_callback,
        );

        // Use the factory function to create the appropriate microphone encoder
        let mut microphone =
            create_microphone_encoder(client.clone(), AUDIO_BITRATE_KBPS, microphone_callback);

        let mut screen = ScreenEncoder::new(client.clone(), SCREEN_BITRATE_KBPS, screen_callback);

        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::VIDEO);
        camera.set_encoder_control(rx);

        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::AUDIO);
        microphone.set_encoder_control(rx);

        let (tx, rx) = mpsc::unbounded();
        client.subscribe_diagnostics(tx.clone(), MediaType::SCREEN);
        screen.set_encoder_control(rx);

        Self {
            camera,
            microphone,
            screen,
            share_screen: ctx.props().share_screen,
            mic_enabled: ctx.props().mic_enabled,
            video_enabled: ctx.props().video_enabled,
            encoder_settings: EncoderSettings {
                camera: None,
                microphone: None,
                screen: None,
            },
            selected_speaker_id: None,
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
        log::debug!("Host update: {:?}", msg);
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
            Msg::AudioDeviceChanged(audio) => {
                if self.microphone.select(audio) {
                    let link = ctx.link().clone();
                    let timeout = Timeout::new(1000, move || {
                        link.send_message(Msg::EnableMicrophone(true));
                    });
                    timeout.forget();
                }
                false
            }
            Msg::VideoDeviceChanged(video) => {
                if self.camera.select(video) {
                    let link = ctx.link().clone();
                    let timeout = Timeout::new(1000, move || {
                        link.send_message(Msg::EnableVideo(true));
                    });
                    timeout.forget();
                }
                false
            }
            Msg::SpeakerDeviceChanged(speaker) => {
                self.selected_speaker_id = Some(speaker);
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
        };
        log::debug!("Host update: {:?}", should_update);
        should_update
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mic_callback = ctx.link().callback(Msg::AudioDeviceChanged);
        let cam_callback = ctx.link().callback(Msg::VideoDeviceChanged);
        let speaker_callback = ctx.link().callback(Msg::SpeakerDeviceChanged);
        html! {
            <>
                <video class="self-camera" autoplay=true id={VIDEO_ELEMENT_ID}></video>
                <DeviceSelector 
                    on_microphone_select={mic_callback} 
                    on_camera_select={cam_callback}
                    on_speaker_select={speaker_callback}
                />
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
