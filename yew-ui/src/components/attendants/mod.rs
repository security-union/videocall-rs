mod model;
mod msg;
mod peer;

use super::host::HostComponent;
use model::{ConnectArgs, Model, State};
use msg::{Msg, WsAction};
use types::protos::rust::media_packet::MediaPacket;
use web_sys::AudioData;
use yew::prelude::*;
use yew::virtual_dom::VNode;
use yew::{html, Component, Context, Html};
use yew_websocket::websocket::WebSocketStatus;

#[derive(Properties, Debug, PartialEq)]
pub struct AttendandsProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub media_packet: MediaPacket,

    #[prop_or_default]
    pub email: String,
}

pub struct AttendandsComponent {
    model: Model,
}

impl Component for AttendandsComponent {
    type Message = Msg;
    type Properties = AttendandsProps;

    fn create(_ctx: &Context<Self>) -> Self {
        AttendandsComponent {
            model: Model::new(),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match (self.model.state(), msg) {
            (_, Msg::WsAction(action)) => match action {
                WsAction::Connect => {
                    let callback = ctx.link().callback(|data| Msg::OnInboundMedia(data));
                    let notification = ctx.link().batch_callback(|status| match status {
                        WebSocketStatus::Opened => Some(WsAction::Connected.into()),
                        WebSocketStatus::Closed | WebSocketStatus::Error => {
                            Some(WsAction::Lost.into())
                        }
                    });
                    let meeting_id = ctx.props().id.clone();
                    let email = ctx.props().email.clone();
                    self.model.connect(ConnectArgs {
                        callback,
                        notification,
                        meeting_id,
                        email,
                    });
                    true
                }
                WsAction::Disconnect => {
                    self.model.disconnect();
                    true
                }
                WsAction::Connected => {
                    self.model.connection_succeed();
                    true
                }
                WsAction::Lost => {
                    self.model.disconnect();
                    false
                }
            },
            (State::Connected, Msg::OnInboundMedia(response)) => {
                let packet = response.0;
                let peer_email = packet.email.clone();
                if !self.model.peer_connected(&peer_email) {
                    self.model.register_peer(packet);
                    return true;
                }
                let peer = self.model.get_peer_mut(&peer_email).unwrap();
                peer.handle_media_packet(packet);
                false
            }
            (State::Connected, Msg::OnOutboundVideoPacket(packet)) => {
                self.model.send_video_packet(packet);
                false
            }
            (State::Connected, Msg::OnOutboundAudioPacket(audio_frame)) => {
                let email = ctx.props().email.clone();
                self.model.send_audio_packet(email, audio_frame);
                false
            }
            (State::Disconnected, Msg::OnInboundMedia(_))
            | (State::Disconnected, Msg::OnOutboundAudioPacket(_))
            | (State::Disconnected, Msg::OnOutboundVideoPacket(_))
            | (State::Created, Msg::OnInboundMedia(_))
            | (State::Created, Msg::OnOutboundAudioPacket(_))
            | (State::Created, Msg::OnOutboundVideoPacket(_))
            | (State::Connecting, Msg::OnInboundMedia(_))
            | (State::Connecting, Msg::OnOutboundVideoPacket(_))
            | (State::Connecting, Msg::OnOutboundAudioPacket(_)) => false,
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let email = ctx.props().email.clone();
        let on_frame = ctx
            .link()
            .callback(|frame: MediaPacket| Msg::OnOutboundVideoPacket(frame));

        let on_audio = ctx
            .link()
            .callback(|frame: AudioData| Msg::OnOutboundAudioPacket(frame));
        let rows: Vec<VNode> = self
            .model
            .connected_peers()
            .iter()
            .map(|(key, _value)| {
                html! {
                    <div class="grid-item">
                        <canvas id={key.clone()}></canvas>
                        <h4 class="floating-name">{key.clone()}</h4>
                    </div>
                }
            })
            .collect();

        let connect_btn_disabled = match self.model.state() {
            State::Connecting | State::Connected => true,
            State::Created | State::Disconnected => false,
        };

        html! {
            <div class="grid-container">
                { rows }
                <nav class="grid-item menu">
                    <div class="controls">
                        <button disabled={connect_btn_disabled}
                                onclick={ctx.link().callback(|_| WsAction::Connect)}>
                            { "Connect" }
                        </button>
                        <button disabled={!connect_btn_disabled}
                                onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                            { "Close" }
                        </button>
                    </div>
                    <HostComponent on_frame={on_frame} on_audio={on_audio} email={email.clone()}/>
                    <h4 class="floating-name">{email}</h4>
                </nav>
            </div>
        }
    }
}
