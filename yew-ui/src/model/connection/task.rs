//
// Generic Task that can be a WebSocketTask or WebTransportTask.
//
// Handles rollover of connection from WebTransport to WebSocket
//
use gloo_console::log;
use types::protos::media_packet::MediaPacket;
use yew_websocket::websocket::WebSocketTask;
use yew_webtransport::webtransport::WebTransportTask;

use super::webmedia::{ConnectOptions, WebMedia};

pub(super) enum Task {
    WebSocket(WebSocketTask),
    WebTransport(WebTransportTask),
}

impl Task {
    pub fn connect(webtransport: bool, options: ConnectOptions) -> anyhow::Result<Self> {
        if webtransport {
            log!("Task::connect trying WebTransport");
            match WebTransportTask::connect(options.clone()) {
                Ok(task) => return Ok(Task::WebTransport(task)),
                Err(_e) => log!("WebTransport connect failed, falling back to WebSocket"),
            }
        }
        log!("Task::connect trying WebSocket");
        WebSocketTask::connect(options.clone()).map(|task| Task::WebSocket(task))
    }

    pub fn send_packet(&self, packet: MediaPacket) {
        match self {
            Task::WebSocket(ws) => ws.send_packet(packet),
            Task::WebTransport(wt) => wt.send_packet(packet),
        }
    }
}
