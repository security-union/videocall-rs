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

// Defines trait giving a consistent interface for making and using connections, at the level of
// MediaPackets
//
// Implemented both for WebSockets (websocket.rs) and WebTransport (webtransport.rs)
//
use log::error;
use protobuf::Message;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::JsValue;

#[derive(Clone)]
pub struct ConnectOptions {
    pub websocket_url: String,
    pub webtransport_url: String,
    pub on_inbound_media: Callback<PacketWrapper>,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<JsValue>,
    pub peer_monitor: Callback<()>,
}

pub(super) trait WebMedia<TASK> {
    fn connect(options: ConnectOptions) -> anyhow::Result<TASK>;

    /// Send bytes via a reliable, ordered unidirectional stream.
    fn send_bytes(&self, bytes: Vec<u8>);

    /// Send bytes via an unreliable, unordered datagram (WebTransport only).
    ///
    /// For transports that do not support datagrams (e.g., WebSocket), this
    /// falls back to the reliable send path.
    fn send_bytes_datagram(&self, bytes: Vec<u8>) {
        // Default implementation falls back to reliable stream.
        // WebTransportTask overrides this to use actual datagrams.
        self.send_bytes(bytes);
    }

    fn send_packet(&self, packet: PacketWrapper) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes(bytes),
            Err(e) => {
                let packet_type = packet.packet_type.enum_value_or_default();
                error!("error sending {packet_type} packet: {e:?}");
            }
        }
    }

    /// Send a packet via datagram if the transport supports it, otherwise
    /// fall back to reliable stream.
    fn send_packet_datagram(&self, packet: PacketWrapper) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes_datagram(bytes),
            Err(e) => {
                let packet_type = packet.packet_type.enum_value_or_default();
                error!("error sending {packet_type} packet via datagram: {e:?}");
            }
        }
    }
}
