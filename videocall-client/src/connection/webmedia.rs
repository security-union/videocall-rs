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
use super::connection_lost_reason::ConnectionLostReason;
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
    pub on_connection_lost: Callback<ConnectionLostReason>,
    pub peer_monitor: Callback<()>,
}

/// Logical media-type identifier used by the WebTransport transport to pick
/// which persistent unidirectional stream a reliable packet rides on.
///
/// One QUIC stream per variant — see Phase 2 of the WebTransport freeze fix
/// (HCL discussion #756).  Mixing audio and video on a single stream causes
/// head-of-line blocking: an uplink stall on a large video keyframe stalls
/// every queued audio packet behind it.  Separating by media type means
/// audio is **never** blocked by video congestion.
///
/// The enum is also used as the on-the-wire bucket discriminator: the
/// numeric `u8` value is opaque to the server (which routes by the
/// `MediaType` field inside the encrypted protobuf payload) but stable
/// across reconnects so that diagnostics can attribute stream-restart
/// events to a media type.
///
/// WebSocket transport ignores this hint — its single TCP stream has no
/// notion of per-media routing.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MediaStreamKey {
    /// Audio packets (mic encoder, ~50 pps).
    Audio,
    /// Camera video packets (~30 pps, delta or keyframe).
    Video,
    /// Screen-share video packets.
    Screen,
    /// Control / signaling: KEYFRAME_REQUEST, RSA_PUB_KEY, AES_KEY,
    /// CONNECTION, HEALTH, DIAGNOSTICS, MEETING, anything not a primary
    /// media stream.
    Control,
}

impl MediaStreamKey {
    /// Stable `u8` representation passed to the transport layer.  Values
    /// are arbitrary but **must not change** without a coordinated
    /// client+server release — they are the wire identity of each
    /// persistent stream.
    pub fn as_u8(self) -> u8 {
        match self {
            MediaStreamKey::Audio => 1,
            MediaStreamKey::Video => 2,
            MediaStreamKey::Screen => 3,
            MediaStreamKey::Control => 4,
        }
    }
}

pub(super) trait WebMedia<TASK> {
    fn connect(options: ConnectOptions) -> anyhow::Result<TASK>;

    /// Send bytes via a reliable, ordered unidirectional stream.
    ///
    /// `stream_key` selects the persistent QUIC stream to ride on.  The
    /// WebTransport implementation maintains one stream per `MediaStreamKey`
    /// to prevent head-of-line blocking across media types.  WebSocket
    /// ignores `stream_key` — its single TCP stream serves everything.
    fn send_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey);

    /// Send bytes via an unreliable, unordered datagram (WebTransport only).
    ///
    /// For transports that do not support datagrams (e.g., WebSocket), this
    /// falls back to the reliable send path.  Datagrams are not keyed by
    /// `MediaStreamKey` — they are a separate primitive used for periodic
    /// expendable traffic (heartbeats, RTT probes).
    fn send_bytes_datagram(&self, bytes: Vec<u8>) {
        // Default implementation falls back to reliable stream.
        // WebTransportTask overrides this to use actual datagrams.
        // Datagram fallback rides on the Control stream so reliable
        // delivery is preserved when the transport does not support
        // datagrams (i.e. WebSocket).
        self.send_bytes(bytes, MediaStreamKey::Control);
    }

    /// Send a packet on the reliable path keyed by `stream_key`.
    ///
    /// Callers must classify each packet up-front: audio → `Audio`,
    /// camera → `Video`, screen-share → `Screen`, everything else →
    /// `Control`.  See call-site updates in `video_call_client.rs`.
    fn send_packet(&self, packet: PacketWrapper, stream_key: MediaStreamKey) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes(bytes, stream_key),
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
