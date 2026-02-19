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

//
// Generic Task that can be a WebSocketTask or WebTransportTask.
//
// Handles rollover of connection from WebTransport to WebSocket
//
use log::{debug, error};
use videocall_transport::websocket::WebSocketTask;
use videocall_transport::webtransport::WebTransportTask;
use videocall_types::protos::packet_wrapper::PacketWrapper;

use super::webmedia::{ConnectOptions, WebMedia};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
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
