use gloo_console::log;
use gloo_timers::callback::Timeout;
use types::protos::packet_wrapper::PacketWrapper;

use std::fmt::Debug;
use yew::prelude::*;

use crate::components::device_selector::DeviceSelector;
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use crate::model::encode::CameraEncoder;
use crate::model::encode::MicrophoneEncoder;
use crate::model::encode::ScreenEncoder;

const VIDEO_ELEMENT_ID: &str = "webcam";

pub enum Msg {
    Start,
    EnableScreenShare,
    EnableMicrophone(bool),
    EnableVideo(bool),
    AudioDeviceChanged(String),
    VideoDeviceChanged(String),
}

pub struct Host {
    pub camera: CameraEncoder,
    pub microphone: MicrophoneEncoder,
    pub screen: ScreenEncoder,
    aes: Aes128State,
    rsa: RsaWrapper,
}

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub on_packet: Callback<PacketWrapper>,

    #[prop_or_default]
    pub email: String,

    pub share_screen: bool,

    pub mic_enabled: bool,

    pub video_enabled: bool,

    pub aes: Aes128State,

    pub rsa: RsaWrapper,
}

impl Component for Host {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(ctx: &Context<Self>) -> Self {
        let aes = ctx.props().aes;
        let rsa = ctx.props().rsa.clone();
        Self {
            camera: CameraEncoder::new(aes),
            microphone: MicrophoneEncoder::new(),
            screen: ScreenEncoder::new(),
            aes,
            rsa,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        // Determine if we should start/stop screen share.
        if self.screen.set_enabled(ctx.props().share_screen) && ctx.props().share_screen {
            ctx.link().send_message(Msg::EnableScreenShare);
        }
        // Determine if we should start/stop microphone.
        if self.microphone.set_enabled(ctx.props().mic_enabled) {
            ctx.link()
                .send_message(Msg::EnableMicrophone(ctx.props().mic_enabled));
        }
        // Determine if we should start/stop video.
        if self.camera.set_enabled(ctx.props().video_enabled) {
            ctx.link()
                .send_message(Msg::EnableVideo(ctx.props().video_enabled));
        }

        if first_render {
            ctx.link().send_message(Msg::Start);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::EnableScreenShare => {
                let on_frame = ctx.props().on_packet.clone();
                let email = ctx.props().email.clone();
                self.screen.start(email, move |packet: PacketWrapper| {
                    on_frame.emit(packet)
                });
                true
            }
            Msg::Start => true,
            Msg::EnableMicrophone(should_enable) => {
                if !should_enable {
                    return true;
                }
                let on_audio = ctx.props().on_packet.clone();
                let email = ctx.props().email.clone();
                self.microphone
                    .start(email, move |packet: PacketWrapper| {
                        on_audio.emit(packet)
                    });
                true
            }
            Msg::EnableVideo(should_enable) => {
                if !should_enable {
                    return true;
                }

                let on_packet = ctx.props().on_packet.clone();
                let email = ctx.props().email.clone();
                self.camera.start(
                    email,
                    move |packet: PacketWrapper| on_packet.emit(packet),
                    VIDEO_ELEMENT_ID,
                );
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
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mic_callback = ctx.link().callback(Msg::AudioDeviceChanged);
        let cam_callback = ctx.link().callback(Msg::VideoDeviceChanged);
        html! {
            <>
                <video class="self-camera" autoplay=true id={VIDEO_ELEMENT_ID}></video>
                <DeviceSelector on_microphone_select={mic_callback} on_camera_select={cam_callback}/>
            </>
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        log!("destroying");
        self.camera.stop();
        self.microphone.stop();
        self.screen.stop();
    }
}
