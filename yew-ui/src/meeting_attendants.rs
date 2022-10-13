use anyhow::{anyhow, Error};
use gloo_console::log;
use protobuf::Message;
use serde_derive::{Deserialize, Serialize};
use types::protos::media_packet::MediaPacket;
use yew_websocket::macros::Json;

use yew::prelude::*;
use yew::{html, Component, Context, Html};
use yew_websocket::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};

use crate::{constants::ACTIX_WEBSOCKET, meeting_self::HostComponent};

pub enum WsAction {
    Connect,
    SendData(),
    Connected,
    Disconnect,
    Lost,
}

pub enum Msg {
    WsAction(WsAction),
    WsReady(Result<WsResponse, Error>),
    OnFrame(MediaPacket),
}

impl From<WsAction> for Msg {
    fn from(action: WsAction) -> Self {
        Msg::WsAction(action)
    }
}

/// This type is used as a request which sent to websocket connection.
#[derive(Serialize, Debug)]
struct WsRequest {
    value: u32,
}

/// This type is an expected response from a websocket connection.
#[derive(Deserialize, Debug)]
pub struct WsResponse {
    value: u32,
}

#[derive(Properties, Debug, PartialEq)]
pub struct AttendandsComponentProps {
    #[prop_or_default]
    pub id: String,

    #[prop_or_default]
    pub media_packet: MediaPacket,
}

pub struct AttendandsComponent {
    pub fetching: bool,
    pub data: Option<u32>,
    pub ws: Option<WebSocketTask>,
    pub media_packet: MediaPacket,
    pub connected: bool,
}

impl Component for AttendandsComponent {
    type Message = Msg;
    type Properties = AttendandsComponentProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            fetching: false,
            data: None,
            ws: None,
            connected: false,
            media_packet: MediaPacket::default(),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::WsAction(action) => match action {
                WsAction::Connect => {
                    let callback = ctx.link().callback(|Json(data)| Msg::WsReady(data));
                    let notification = ctx.link().batch_callback(|status| match status {
                        WebSocketStatus::Opened => Some(WsAction::Connected.into()),
                        WebSocketStatus::Closed | WebSocketStatus::Error => {
                            Some(WsAction::Lost.into())
                        }
                    });
                    let meeting_id = ctx.props().id.clone();
                    let url = format!("{}{}", ACTIX_WEBSOCKET.to_string(), meeting_id);
                    let task = WebSocketService::connect(&url, callback, notification).unwrap();
                    self.ws = Some(task);
                    true
                }
                WsAction::SendData() => {
                    let media = MediaPacket::default();
                    let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                    self.ws.as_mut().unwrap().send_binary(bytes);
                    false
                }
                WsAction::Disconnect => {
                    self.ws.take();
                    self.connected = false;
                    true
                }
                WsAction::Connected => {
                    self.connected = true;
                    true
                }
                WsAction::Lost => {
                    self.ws = None;
                    self.connected = false;
                    true
                }
            },
            Msg::WsReady(response) => {
                self.data = response.map(|data| data.value).ok();
                true
            }
            Msg::OnFrame(media) => {
                // Send image to the server.
                if let Some(ws) = self.ws.as_mut() {
                    if self.connected {
                        let bytes = media.write_to_bytes().map_err(|w| anyhow!("{:?}", w));
                        ws.send_binary(bytes);
                    } else {
                        log!("disconnected");
                    }
                } else {
                    // log!("No websocket!!!!");
                }
                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let media_packet = ctx.props().media_packet.clone();
        let on_frame = ctx.link().callback(|frame: MediaPacket| {
            // log!("on meeting attendant callback");
            Msg::OnFrame(frame)
        });
        html! {
            <div>
                <nav class="menu">
                    <button disabled={self.ws.is_some()}
                            onclick={ctx.link().callback(|_| WsAction::Connect)}>
                        { "Connect To WebSocket" }
                    </button>
                    <button disabled={self.ws.is_none()}
                            onclick={ctx.link().callback(|_| WsAction::SendData())}>
                        { "Send To WebSocket [binary]" }
                    </button>
                    <button disabled={self.ws.is_none()}
                            onclick={ctx.link().callback(|_| WsAction::Disconnect)}>
                        { "Close WebSocket connection" }
                    </button>
                </nav>
                <HostComponent media_packet={media_packet} on_frame={on_frame}/>
            </div>
        }
    }
}
