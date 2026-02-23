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

/// Hint for how a packet should be transported.
///
/// When using WebTransport, `Datagram` sends via unreliable datagrams (low latency, no
/// head-of-line blocking, but may be dropped). `Reliable` sends via a new unidirectional
/// stream (guaranteed delivery, ordered).
///
/// WebSocket ignores the hint and always sends reliably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportHint {
    /// Send via a reliable, ordered channel (unidirectional stream / WebSocket).
    Reliable,
    /// Send via an unreliable datagram (WebTransport only; falls back to reliable on WebSocket).
    Datagram,
}

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
    fn send_bytes(&self, bytes: Vec<u8>);

    /// Send raw bytes with a transport hint. The default implementation ignores the hint and
    /// delegates to [`send_bytes`]. WebTransport overrides this to route datagrams.
    fn send_bytes_with_hint(&self, bytes: Vec<u8>, _hint: TransportHint) {
        self.send_bytes(bytes);
    }

    fn send_packet_with_hint(&self, packet: PacketWrapper, hint: TransportHint) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes_with_hint(bytes, hint),
            Err(e) => {
                let packet_type = packet.packet_type.enum_value_or_default();
                error!("error sending {packet_type} packet: {e:?}");
            }
        }
    }
}
