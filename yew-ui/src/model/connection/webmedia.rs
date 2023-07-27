// Defines trait giving a consistent interface for making and using connections, at the level of
// MediaPackets
//
// Implemented both for WebSockets (websocket.rs) and WebTransport (webtransport.rs)
//
use gloo_console::log;
use protobuf::Message;
use types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::JsValue;
use yew::prelude::Callback;

#[derive(Clone)]
pub struct ConnectOptions {
    pub userid: String,
    pub websocket_url: String,
    pub webtransport_url: String,
    pub on_inbound_media: Callback<PacketWrapper>,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<()>,
}

pub(super) trait WebMedia<TASK> {
    fn connect(options: ConnectOptions) -> anyhow::Result<TASK>;
    fn send_bytes(&self, bytes: Vec<u8>);

    fn send_packet(&self, packet: PacketWrapper) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes(bytes),
            Err(e) => {
                let packet_type = packet.packet_type.enum_value_or_default();
                log!(
                    "error sending {} packet: {:?}",
                    JsValue::from(format!("{packet_type}")),
                    e
                );
            }
        }
    }
}
