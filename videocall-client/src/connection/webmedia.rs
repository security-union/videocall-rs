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
use videocall_transport::webtransport::FrameDropMeta;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen::JsValue;

/// Re-exported from the transport crate, which owns it since #2728 moved the
/// inbound framing into a module the session Worker also links.
pub use videocall_transport::inbound::{InboundLane, ReceivedAtMs};

#[derive(Clone)]
pub struct ConnectOptions {
    pub websocket_url: String,
    pub webtransport_url: String,
    /// Every inbound packet, with the lane that carried it and the instant
    /// the TRANSPORT received it. #2728 made that instant distinct from "now":
    /// under a main-thread stall the Worker received the packet seconds before
    /// this callback runs.
    pub on_inbound_media: Callback<(PacketWrapper, InboundLane, ReceivedAtMs)>,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<ConnectionLostReason>,
    pub peer_monitor: Callback<()>,
    pub adopt_wt_spare_worker: bool,
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
/// WebSocket transport ignores this hint for routing — its single TCP stream
/// has no per-media lanes — but retains it for drop-counter attribution.
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
    pub const fn as_u8(self) -> u8 {
        match self {
            MediaStreamKey::Audio => 1,
            MediaStreamKey::Video => 2,
            MediaStreamKey::Screen => 3,
            MediaStreamKey::Control => 4,
        }
    }

    /// Highest currently assigned `MediaStreamKey` wire value.
    pub const MAX_WIRE_VALUE: u8 = videocall_types::limits::MAX_MEDIA_STREAM_KEY;

    /// QUIC scheduling hint for this key's persistent uplink unistream, passed
    /// as `WebTransportSendStreamOptions.sendOrder` at stream creation (#2722).
    /// The W3C spec defines a HIGHER value as sent first; with no value the
    /// four streams carry no relative priority at all, so one screen keyframe
    /// competes with a second of audio on a constrained uplink.
    ///
    /// Audio first (unconcealable delay), then Control (mostly tiny packets that
    /// gate something large — the #2721 RTT probe, KEYFRAME_REQUEST, AES_KEY —
    /// though oversized datagrams also fall back onto it), then Screen, then
    /// camera Video LAST: camera is the adaptive source, stepping its own tier
    /// down on its own stream's stalls, while screen has neither a send-side
    /// drop nor an age-out on WebTransport and is the content when someone is
    /// presenting. Values are spaced so a future lane can be inserted between
    /// any two; only their ORDER is load-bearing.
    pub const fn send_order(self) -> i32 {
        match self {
            MediaStreamKey::Audio => 300,
            MediaStreamKey::Control => 200,
            MediaStreamKey::Screen => 150,
            MediaStreamKey::Video => 100,
        }
    }
}

const _: () = assert!(MediaStreamKey::Control.as_u8() == MediaStreamKey::MAX_WIRE_VALUE);

pub(super) trait WebMedia<TASK> {
    fn connect(options: ConnectOptions) -> anyhow::Result<TASK>;

    /// Send bytes via a reliable, ordered unidirectional stream.
    ///
    /// `stream_key` selects the persistent QUIC stream to ride on.  The
    /// WebTransport implementation maintains one stream per `MediaStreamKey`
    /// to prevent head-of-line blocking across media types. WebSocket uses one
    /// TCP stream for every key and retains the key only for counter attribution.
    fn send_bytes(&self, bytes: Vec<u8>, stream_key: MediaStreamKey);

    fn send_bytes_with_drop_meta(
        &self,
        bytes: Vec<u8>,
        stream_key: MediaStreamKey,
        _meta: Option<FrameDropMeta>,
    ) {
        self.send_bytes(bytes, stream_key);
    }

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
        self.send_packet_with_drop_meta(packet, stream_key, None);
    }

    fn send_packet_with_drop_meta(
        &self,
        packet: PacketWrapper,
        stream_key: MediaStreamKey,
        meta: Option<FrameDropMeta>,
    ) {
        match packet
            .write_to_bytes()
            .map_err(|w| JsValue::from(format!("{w:?}")))
        {
            Ok(bytes) => self.send_bytes_with_drop_meta(bytes, stream_key, meta),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Highest first. Only the ORDER is a contract, so the tests assert the
    const RANKED_HIGHEST_FIRST: [MediaStreamKey; 4] = [
        MediaStreamKey::Audio,
        MediaStreamKey::Control,
        MediaStreamKey::Screen,
        MediaStreamKey::Video,
    ];

    #[test]
    fn send_order_is_strictly_descending_from_audio_to_camera_video() {
        assert_eq!(
            RANKED_HIGHEST_FIRST.len(),
            MediaStreamKey::MAX_WIRE_VALUE as usize,
            "every MediaStreamKey must be ranked; a new variant belongs in this list"
        );
        for pair in RANKED_HIGHEST_FIRST.windows(2) {
            assert!(
                pair[0].send_order() > pair[1].send_order(),
                "{:?} must be scheduled before {:?}, got {} vs {}",
                pair[0],
                pair[1],
                pair[0].send_order(),
                pair[1].send_order(),
            );
        }
    }

    #[test]
    fn every_media_stream_key_has_its_own_send_order() {
        let mut orders: Vec<i32> = RANKED_HIGHEST_FIRST
            .iter()
            .map(|k| k.send_order())
            .collect();
        orders.sort_unstable();
        orders.dedup();
        assert_eq!(
            orders.len(),
            RANKED_HIGHEST_FIRST.len(),
            "each persistent uplink stream needs a sendOrder of its own"
        );
    }

    #[test]
    fn audio_outranks_and_camera_video_yields_to_every_other_stream() {
        for key in RANKED_HIGHEST_FIRST {
            if key != MediaStreamKey::Audio {
                assert!(
                    MediaStreamKey::Audio.send_order() > key.send_order(),
                    "audio must outrank {key:?}"
                );
            }
            if key != MediaStreamKey::Video {
                assert!(
                    MediaStreamKey::Video.send_order() < key.send_order(),
                    "camera video must yield to {key:?}"
                );
            }
        }
    }
}
