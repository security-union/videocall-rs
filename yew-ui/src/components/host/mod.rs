mod model;
mod msg;

use model::{Model, StartStreamingArgs};
use msg::Msg;
use std::fmt::Debug;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use types::protos::media_packet::MediaPacket;
use web_sys::*;
use yew::prelude::*;

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub on_frame: Callback<MediaPacket>,

    #[prop_or_default]
    pub on_audio: Callback<AudioData>,

    #[prop_or_default]
    pub email: String,
}

pub struct HostComponent {
    destroy: Arc<AtomicBool>,
}

impl Component for HostComponent {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            destroy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(Msg::Start);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::Start => {
                Model::start(StartStreamingArgs {
                    on_frame: (ctx.props().on_frame.clone()),
                    on_audio: (ctx.props().on_audio.clone()),
                    email: (ctx.props().email.clone()),
                    destroy: self.destroy.clone(),
                });
                true
            }
        }
    }

    fn view(&self, _ctx: &Context<Self>) -> Html {
        html! {
            <video class="self-camera" autoplay=true id="webcam"></video>
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        self.destroy.store(true, Ordering::Release);
    }
}
