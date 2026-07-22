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

pub mod callback;
pub mod feature_flags;
pub mod protos;
pub mod user_id;
pub mod validation;

pub use callback::Callback;
pub use feature_flags::FeatureFlags;
use protobuf::Message;
pub use user_id::{is_system_user, to_user_id_bytes, user_id_bytes_to_string};

/// A representation of a value which can be stored and restored as a text.
pub type Text = Result<String, anyhow::Error>;

/// A representation of a value which can be stored and restored as a binary.
pub type Binary = Result<Vec<u8>, anyhow::Error>;

/// System user ID used for server-generated messages (meeting info, meeting started/ended).
/// This is not a real user and should be filtered out in UI/peer management.
pub const SYSTEM_USER_ID: &str = "system-&^%$#@!";

/// `PeerEvent.event_type` value emitted by a peer the first time it decodes
/// a screen-share frame from a remote publisher. Used by the publisher's UI
/// to confirm that its shared content is actually visible to at least one
/// other peer (HCL issue #893).
///
/// Producers and consumers MUST use this constant so the string is checked
/// at one source of truth.
pub const PEER_EVENT_SCREEN_DECODE_STARTED: &str = "screen_decode_started";

/// `PeerEvent.event_type` broadcast to all room participants when a peer
/// starts recording the meeting. Consumers display an informational banner.
pub const PEER_EVENT_RECORDING_STARTED: &str = "recording_started";

/// `PeerEvent.event_type` broadcast to all room participants when a peer
/// stops recording the meeting.
pub const PEER_EVENT_RECORDING_STOPPED: &str = "recording_stopped";

impl std::fmt::Display for protos::media_packet::media_packet::MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::media_packet::media_packet::MediaType::MEDIA_TYPE_UNKNOWN => {
                write!(f, "UNKNOWN")
            }
            protos::media_packet::media_packet::MediaType::AUDIO => write!(f, "audio"),
            protos::media_packet::media_packet::MediaType::VIDEO => write!(f, "video"),
            protos::media_packet::media_packet::MediaType::SCREEN => write!(f, "screen"),
            protos::media_packet::media_packet::MediaType::HEARTBEAT => write!(f, "heartbeat"),
            protos::media_packet::media_packet::MediaType::RTT => write!(f, "rtt"),
            protos::media_packet::media_packet::MediaType::KEYFRAME_REQUEST => {
                write!(f, "keyframe_request")
            }
        }
    }
}

impl std::fmt::Display for protos::packet_wrapper::packet_wrapper::PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            protos::packet_wrapper::packet_wrapper::PacketType::PACKET_TYPE_UNKNOWN => {
                write!(f, "UNKNOWN")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::AES_KEY => write!(f, "AES_KEY"),
            protos::packet_wrapper::packet_wrapper::PacketType::RSA_PUB_KEY => {
                write!(f, "RSA_PUB_KEY")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEDIA => write!(f, "MEDIA"),
            protos::packet_wrapper::packet_wrapper::PacketType::CONNECTION => {
                write!(f, "CONNECTION")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::DIAGNOSTICS => {
                write!(f, "DIAGNOSTICS")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::HEALTH => {
                write!(f, "HEALTH")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEETING => {
                write!(f, "MEETING")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::SESSION_ASSIGNED => {
                write!(f, "SESSION_ASSIGNED")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::CONGESTION => {
                write!(f, "CONGESTION")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::PEER_EVENT => {
                write!(f, "PEER_EVENT")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::VIEWPORT => {
                write!(f, "VIEWPORT")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::LAYER_PREFERENCE => {
                write!(f, "LAYER_PREFERENCE")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::LAYER_HINT => {
                write!(f, "LAYER_HINT")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::DOWNLINK_CONGESTION => {
                write!(f, "DOWNLINK_CONGESTION")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::REACTION => {
                write!(f, "REACTION")
            }
        }
    }
}

impl From<Text> for protos::packet_wrapper::PacketWrapper {
    fn from(t: Text) -> Self {
        protos::packet_wrapper::PacketWrapper::parse_from_bytes(&t.unwrap().into_bytes()).unwrap()
    }
}

impl From<Binary> for protos::packet_wrapper::PacketWrapper {
    fn from(bin: Binary) -> Self {
        protos::packet_wrapper::PacketWrapper::parse_from_bytes(&bin.unwrap()).unwrap()
    }
}

pub fn truthy(s: Option<&str>) -> bool {
    if let Some(s) = s {
        ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
    } else {
        false
    }
}

#[cfg(test)]
mod video_stats_wire_tests {
    //! Wire-format round-trip coverage for `VideoStats` (issue #1641).
    //!
    //! `content_staleness_ms` was added as proto field 9 (tag 73) on the existing `VideoStats`
    //! message. These tests exercise the REAL generated encode/decode path
    //! (`protobuf::Message::{write_to_bytes, parse_from_bytes}`) — not an in-memory field set — so
    //! they fail if the field is mis-tagged, dropped, or read with the wrong wire type, and they
    //! pin the proto3 backward-compatibility default for peers that predate the field.
    use crate::protos::health_packet::VideoStats;
    use protobuf::Message;

    #[test]
    fn content_staleness_ms_survives_wire_round_trip() {
        let mut vs = VideoStats::new();
        // A multi-minute content age — the unbounded staleness this metric exists to carry
        // (> the 1800ms playout-latency cap), so the value is unmistakable on the far side.
        vs.content_staleness_ms = 5000.0;

        let bytes = vs
            .write_to_bytes()
            .expect("VideoStats must serialize to protobuf bytes");
        let decoded =
            VideoStats::parse_from_bytes(&bytes).expect("serialized VideoStats must parse back");

        // Mutation sensitivity: if field 9 were mis-tagged, dropped, or decoded with the wrong
        // wire type, this read would not return 5000.0.
        assert_eq!(
            decoded.content_staleness_ms, 5000.0,
            "content_staleness_ms (field 9) must round-trip through the wire unchanged"
        );
    }

    #[test]
    fn content_staleness_ms_defaults_to_zero_when_field_absent() {
        // Serialize a VideoStats that sets ONLY field 1 (fps_received) and leaves field 9 at its
        // proto3 default. proto3 omits default-valued scalars from the wire, so the encoded bytes
        // carry NO field-9 entry — exactly what a peer built before #1641 would send.
        let mut older_peer = VideoStats::new();
        older_peer.fps_received = 30.0;
        assert_eq!(
            older_peer.content_staleness_ms, 0.0,
            "precondition: field 9 left at proto3 default so it is omitted from the wire"
        );

        let bytes = older_peer
            .write_to_bytes()
            .expect("VideoStats must serialize to protobuf bytes");
        let decoded = VideoStats::parse_from_bytes(&bytes)
            .expect("a field-9-less VideoStats must still parse (wire-compatible additive field)");

        assert_eq!(
            decoded.fps_received, 30.0,
            "the field that WAS set must survive"
        );
        assert_eq!(
            decoded.content_staleness_ms, 0.0,
            "a VideoStats without field 9 must decode content_staleness_ms as the proto3 default 0.0"
        );
    }
}

#[cfg(test)]
mod reaction_packet_wire_tests {
    //! Wire-format round-trip coverage for `ReactionPacket` and the
    //! `PacketWrapper.PacketType::REACTION = 17` envelope discriminant (issue #1884).
    //!
    //! These exercise the REAL generated encode/decode path
    //! (`protobuf::Message::{write_to_bytes, parse_from_bytes}`) — not an in-memory field set —
    //! so they fail if `reaction` (field 1, tag 8) or `display_name` (field 2, tag 18) is
    //! mis-tagged, dropped, or read with the wrong wire type, and they pin the closed-enum
    //! contract the relay's ingress allowlist depends on.
    use crate::protos::packet_wrapper::packet_wrapper::PacketType;
    use crate::protos::reaction_packet::reaction_packet::ReactionType;
    use crate::protos::reaction_packet::ReactionPacket;
    use protobuf::{Enum, EnumOrUnknown, Message};

    /// Every defined reaction (1..=7) survives a wire round-trip as the same enum value.
    #[test]
    fn all_defined_reactions_survive_wire_round_trip() {
        let all = [
            ReactionType::THUMBS_UP,
            ReactionType::THUMBS_DOWN,
            ReactionType::LAUGH,
            ReactionType::APPLAUSE,
            ReactionType::HEART,
            ReactionType::THINKING,
            ReactionType::PARTY,
        ];
        // Guard against a future enum edit silently shrinking the covered set: the design pins
        // exactly 7 broadcastable reactions in v1.
        assert_eq!(all.len(), 7, "expected exactly 7 defined reactions in v1");

        for r in all {
            let mut pkt = ReactionPacket::new();
            pkt.reaction = EnumOrUnknown::new(r);
            let bytes = pkt
                .write_to_bytes()
                .expect("ReactionPacket must serialize to protobuf bytes");
            let decoded = ReactionPacket::parse_from_bytes(&bytes)
                .expect("serialized ReactionPacket must parse back");
            assert_eq!(
                decoded.reaction.enum_value(),
                Ok(r),
                "reaction {r:?} (field 1) must round-trip through the wire unchanged"
            );
        }
    }

    /// The cosmetic `display_name` bytes survive the wire alongside the reaction. The wire
    /// preserves the full byte string (the <=64 cap is a client-render concern, not a wire one),
    /// so a name longer than the cap must still decode intact here.
    #[test]
    fn display_name_survives_wire_round_trip() {
        let mut pkt = ReactionPacket::new();
        pkt.reaction = EnumOrUnknown::new(ReactionType::HEART);
        // 80 bytes: deliberately longer than the client's 64-byte render cap to prove the wire
        // itself does not truncate — capping happens on the render side, not here.
        let name = "x".repeat(80).into_bytes();
        pkt.display_name = name.clone();

        let bytes = pkt
            .write_to_bytes()
            .expect("ReactionPacket must serialize to protobuf bytes");
        let decoded = ReactionPacket::parse_from_bytes(&bytes)
            .expect("serialized ReactionPacket must parse back");

        assert_eq!(
            decoded.reaction.enum_value(),
            Ok(ReactionType::HEART),
            "reaction must survive alongside a display_name"
        );
        assert_eq!(
            decoded.display_name, name,
            "display_name (field 2) must round-trip through the wire unchanged"
        );
    }

    /// A reaction the wire carries that is NOT in the closed enum (e.g. a newer client using a
    /// reserved value, or a forged value) decodes as `EnumOrUnknown::Unknown` — the exact
    /// signal the relay ingress and the client consume path key their "drop unknown" branch on.
    #[test]
    fn unknown_reaction_value_decodes_as_unknown() {
        let mut pkt = ReactionPacket::new();
        // 99 is outside the defined 0..=7 range (and the reserved 8..=31 band).
        pkt.reaction = EnumOrUnknown::from_i32(99);

        let bytes = pkt
            .write_to_bytes()
            .expect("ReactionPacket with an unknown enum must serialize");
        let decoded = ReactionPacket::parse_from_bytes(&bytes)
            .expect("serialized ReactionPacket must parse back");

        // `enum_value()` returns Err(raw) for a value the closed enum does not define — this is
        // the drop signal. `from_i32(99)` is None for the same reason.
        assert_eq!(
            decoded.reaction.enum_value(),
            Err(99),
            "an unknown reaction value must decode as EnumOrUnknown::Unknown(99), never a defined variant"
        );
        assert_eq!(
            ReactionType::from_i32(99),
            None,
            "99 is not a defined ReactionType"
        );
    }

    /// The envelope discriminant is pinned at 17 (15/16 reserved for the unmerged #1843). If a
    /// future edit renumbers REACTION this fails, catching a silent wire-compat break with peers
    /// and the relay's classify arm.
    #[test]
    fn reaction_packet_type_is_wire_value_17() {
        assert_eq!(
            PacketType::REACTION.value(),
            17,
            "PacketType::REACTION must be wire value 17 (15/16 reserved for #1843)"
        );
    }
}
