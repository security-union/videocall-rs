//
// Generic Task that can be a WebSocketTask or WebTransportTask.
//
// Handles rollover of connection from WebTransport to WebSocket
//
use log::{debug, error};
use types::protos::packet_wrapper::PacketWrapper;
use yew_websocket::websocket::WebSocketTask;
use yew_webtransport::webtransport::WebTransportTask;

use super::webmedia::{ConnectOptions, WebMedia};

#[derive(Debug)]
pub(super) enum Task {
    WebSocket(WebSocketTask),
    WebTransport(WebTransportTask),
}

impl Task {
    pub fn connect(webtransport: bool, options: ConnectOptions) -> anyhow::Result<Self> {
        if webtransport {
            debug!("Task::connect trying WebTransport");
            match WebTransportTask::connect(options.clone()) {
                Ok(task) => return Ok(Task::WebTransport(task)),
                Err(_e) => error!("WebTransport connect failed, falling back to WebSocket"),
            }
        }
        debug!("Task::connect trying WebSocket");
        WebSocketTask::connect(options).map(Task::WebSocket)
    }

    pub fn send_packet(&self, packet: PacketWrapper) {
        match self {
            Task::WebSocket(ws) => ws.send_packet(packet),
            Task::WebTransport(wt) => wt.send_packet(packet),
        }
    }
}
