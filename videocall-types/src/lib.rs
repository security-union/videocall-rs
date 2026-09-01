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
pub mod limits;
pub mod protos;
pub mod url_log;
pub mod user_id;
pub mod validation;

pub use callback::Callback;
pub use feature_flags::{FeatureFlags, ResolvedFlag};
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
            protos::packet_wrapper::packet_wrapper::PacketType::RAISE_HAND => {
                write!(f, "RAISE_HAND")
            }
            protos::packet_wrapper::packet_wrapper::PacketType::MEETING_TIMER => {
                write!(f, "MEETING_TIMER")
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

    /// Issue #2201: `keyframe_arrivals_total` (field 10, tag 80) must round-trip, and — unlike
    /// its implicit-presence siblings — must preserve EXPLICIT PRESENCE.
    ///
    /// The `optional` keyword is load-bearing for a staged rollout, not stylistic. With implicit
    /// presence a pre-#2201 client reports 0, which is numerically identical to the single most
    /// alarming real condition this metric exists to detect ("we requested keyframes and none
    /// arrived") — so every un-upgraded receiver in a mixed-version meeting would read as total
    /// keyframe delivery loss. Explicit presence lets the server distinguish "not reported" from
    /// "zero arrived" and omit the series entirely.
    ///
    /// This exercises the REAL generated codec rather than an in-memory field set, so it fails
    /// if the field is mis-tagged, loses its `optional` (collapsing `None` and `Some(0)`), or is
    /// decoded with the wrong wire type. `Some(0)` is asserted specifically because that is the
    /// case implicit presence would silently destroy.
    #[test]
    fn keyframe_arrivals_total_round_trips_and_preserves_presence() {
        // A set, nonzero value survives.
        let mut vs = VideoStats::new();
        vs.keyframe_arrivals_total = Some(42);
        let decoded =
            VideoStats::parse_from_bytes(&vs.write_to_bytes().expect("VideoStats must serialize"))
                .expect("serialized VideoStats must parse back");
        assert_eq!(
            decoded.keyframe_arrivals_total,
            Some(42),
            "field 10 must round-trip its value unchanged"
        );

        // Some(0) must stay Some(0) — the discriminating case. Under implicit presence this
        // would be omitted from the wire and decode as the default, indistinguishable from an
        // old client that never reports the field at all.
        let mut zero = VideoStats::new();
        zero.keyframe_arrivals_total = Some(0);
        let decoded_zero = VideoStats::parse_from_bytes(
            &zero.write_to_bytes().expect("VideoStats must serialize"),
        )
        .expect("serialized VideoStats must parse back");
        assert_eq!(
            decoded_zero.keyframe_arrivals_total,
            Some(0),
            "an explicitly-reported ZERO must survive as Some(0), not collapse to None — that \
             distinction is what stops an old client from reading as total delivery loss"
        );

        // An OLD client (field never set) must decode as None, not Some(0).
        let mut older_peer = VideoStats::new();
        older_peer.fps_received = 8.0;
        assert_eq!(
            older_peer.keyframe_arrivals_total, None,
            "precondition: field 10 unset"
        );
        let decoded_old = VideoStats::parse_from_bytes(
            &older_peer
                .write_to_bytes()
                .expect("VideoStats must serialize"),
        )
        .expect("a field-10-less VideoStats must still parse (additive field)");
        assert_eq!(
            decoded_old.fps_received, 8.0,
            "the field that WAS set must survive"
        );
        assert_eq!(
            decoded_old.keyframe_arrivals_total, None,
            "a pre-#2201 peer must decode as None so the server can OMIT the series rather \
             than publish a 0 that reads as total keyframe delivery loss"
        );
    }

    /// Issue 2511: fields 11-14 round-trip with explicit presence, and field 14 is a
    /// VARINT — changing that once clients ship it would require a new field number.
    #[test]
    fn freeze_family_round_trips_and_field_14_is_a_varint() {
        let mut vs = VideoStats::new();
        vs.freeze_episodes_total = Some(3);
        vs.freeze_ms_total = Some(7_400);
        vs.max_decode_gap_ms = Some(5_100);
        vs.max_content_staleness_ms = Some(0);

        let bytes = vs.write_to_bytes().expect("VideoStats must serialize");
        let decoded = VideoStats::parse_from_bytes(&bytes).expect("must parse back");
        assert_eq!(decoded.freeze_episodes_total, Some(3));
        assert_eq!(decoded.freeze_ms_total, Some(7_400));
        assert_eq!(decoded.max_decode_gap_ms, Some(5_100));
        assert_eq!(
            decoded.max_content_staleness_ms,
            Some(0),
            "explicit presence: Some(0) must not collapse to None, or a genuine \
             'observed, and it was 0' becomes indistinguishable from a pre-2511 client"
        );

        let mut only_14 = VideoStats::new();
        only_14.max_content_staleness_ms = Some(4_800);
        let bytes = only_14.write_to_bytes().expect("VideoStats must serialize");
        assert_eq!(
            bytes[0], 0x70,
            "field 14 must carry wire type 0 (varint); 0x71 means it went back to double"
        );
        assert_eq!(
            bytes.len(),
            3,
            "4800 is a 2-byte varint plus 1 tag byte; 9 bytes means a fixed64 double: {bytes:?}"
        );
    }

    /// Issue 2524: fields 15-17 round-trip with explicit presence, and a pre-2524 peer decodes
    /// them as absent so the server omits the series instead of publishing a 0 that reads as
    /// "no burst" / "never froze".
    #[test]
    fn loss_and_freshness_fields_round_trip_with_explicit_presence() {
        let mut vs = VideoStats::new();
        vs.max_seq_gap_frames = Some(437);
        vs.freshness_evictions_total = Some(31);
        vs.freshness_evictions_keyframeless_total = Some(0);

        let bytes = vs.write_to_bytes().expect("VideoStats must serialize");
        let decoded = VideoStats::parse_from_bytes(&bytes).expect("must parse back");
        assert_eq!(decoded.max_seq_gap_frames, Some(437));
        assert_eq!(decoded.freshness_evictions_total, Some(31));
        assert_eq!(
            decoded.freshness_evictions_keyframeless_total,
            Some(0),
            "explicit presence: Some(0) must not collapse to None, or 'observed, and it was \
             0' becomes indistinguishable from a pre-2524 client"
        );

        let old = VideoStats::new();
        let old_bytes = old.write_to_bytes().expect("must serialize");
        let decoded_old = VideoStats::parse_from_bytes(&old_bytes).expect("must parse back");
        assert_eq!(decoded_old.max_seq_gap_frames, None);
        assert_eq!(decoded_old.freshness_evictions_total, None);
        assert_eq!(decoded_old.freshness_evictions_keyframeless_total, None);

        // Tag AND payload: a `sint64` retype keeps the tag, zigzags the payload, and
        // round-trips cleanly — only pinning the bytes catches it.
        for (field, want) in [
            (15u8, vec![0x78u8, 0xB5, 0x03]),
            (16, vec![0x80, 0x01, 0xB5, 0x03]),
            (17, vec![0x88, 0x01, 0xB5, 0x03]),
        ] {
            let mut only = VideoStats::new();
            match field {
                15 => only.max_seq_gap_frames = Some(437),
                16 => only.freshness_evictions_total = Some(437),
                _ => only.freshness_evictions_keyframeless_total = Some(437),
            }
            let b = only.write_to_bytes().expect("must serialize");
            assert_eq!(b, want, "field {field} must be tag + varint(437)");
        }
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

    /// Every defined reaction (1..=12) survives a wire round-trip as the same enum value.
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
            ReactionType::CRY,
            ReactionType::DISAGREE,
            ReactionType::SAD,
            ReactionType::HEART_BROKEN,
            ReactionType::CUSTOM,
        ];
        // Guard against a future enum edit silently shrinking the covered set: the design pins
        // exactly 12 broadcastable reactions (7 originals + 4 negatives + CUSTOM).
        assert_eq!(all.len(), 12, "expected exactly 12 defined reactions");

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

    /// A CUSTOM reaction's `custom_emoji` bytes (field 3, tag 26) survive the wire alongside the
    /// reaction. The wire preserves the raw bytes; the exact-emoji allowlist + <=32-byte cap is a
    /// relay-ingress / client-validation concern, not a wire one, so an over-cap payload still
    /// round-trips intact here (the validators reject it downstream, not the wire).
    #[test]
    fn custom_emoji_survives_wire_round_trip() {
        let mut pkt = ReactionPacket::new();
        pkt.reaction = EnumOrUnknown::new(ReactionType::CUSTOM);
        let emoji = "😭".as_bytes().to_vec();
        pkt.custom_emoji = emoji.clone();

        let bytes = pkt
            .write_to_bytes()
            .expect("ReactionPacket must serialize to protobuf bytes");
        let decoded = ReactionPacket::parse_from_bytes(&bytes)
            .expect("serialized ReactionPacket must parse back");

        assert_eq!(
            decoded.reaction.enum_value(),
            Ok(ReactionType::CUSTOM),
            "CUSTOM reaction must survive alongside a custom_emoji"
        );
        assert_eq!(
            decoded.custom_emoji, emoji,
            "custom_emoji (field 3) must round-trip through the wire unchanged"
        );
    }

    /// A reaction the wire carries that is NOT in the closed enum (e.g. a newer client using a
    /// reserved value, or a forged value) decodes as `EnumOrUnknown::Unknown` — the exact
    /// signal the relay ingress and the client consume path key their "drop unknown" branch on.
    #[test]
    fn unknown_reaction_value_decodes_as_unknown() {
        let mut pkt = ReactionPacket::new();
        // 99 is outside the defined 0..=12 range (and the reserved 13..=31 band).
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
        // 13 is the first RESERVED value after CUSTOM (12): it must also decode as Unknown, pinning
        // the closed-enum boundary so a future client can't accidentally define a value the relay
        // still treats as drop-worthy.
        assert_eq!(
            ReactionType::from_i32(13),
            None,
            "13 is reserved (13..=31), not a defined ReactionType"
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

    /// #2136: the MEETING_TIMER envelope discriminant is pinned at 19. If a future edit
    /// renumbers it, this fails — catching a silent wire-compat break with peers and with the
    /// relay's `classify_packet` arm before it ships.
    ///
    /// The second assert is the part that actually earns its keep: it pins 19 as DISTINCT from
    /// MEETING (7). The two names are adjacent and their trust models are opposite (MEETING is
    /// server-authored and dropped on client ingress; MEETING_TIMER is client-authored and
    /// re-broadcast), so a collision would silently route host timers into the drop arm — or,
    /// far worse, route forged client MEETING packets into the re-broadcast arm.
    #[test]
    fn meeting_timer_packet_type_is_wire_value_19_and_distinct_from_meeting() {
        assert_eq!(
            PacketType::MEETING_TIMER.value(),
            19,
            "PacketType::MEETING_TIMER must be wire value 19 (15/16 reserved for #1843, \
             17 = REACTION, 18 = RAISE_HAND per #2135)"
        );
        assert_ne!(
            PacketType::MEETING_TIMER.value(),
            PacketType::MEETING.value(),
            "MEETING_TIMER (client-authored, re-broadcast) must never collide with MEETING \
             (server-authored, dropped on client ingress) — they have opposite trust models"
        );
    }
}
