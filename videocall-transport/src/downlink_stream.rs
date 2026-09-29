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

//! Wire contract for the per-key WebTransport downlink (#2723 and #2724,
//! protocol v1).

/// Query parameter the client appends to the **WebTransport** connect URL to
/// advertise the downlink protocol version it understands.
///
/// It is the earliest mechanism in the handshake: the relay reads it from the
/// request URI before it accepts the session, hence before it can write any
/// downlink byte. Absent, empty, non-numeric or `0` selects the legacy single
/// shared stream. WebSocket has no downlink streams and never carries it.
pub const DOWNLINK_STREAMS_QUERY: &str = "ds=1";

/// First four bytes of a v1 stream header.
pub const STREAM_HEADER_MAGIC: [u8; 4] = *b"VCDS";

/// The only downlink protocol version this client parses.
pub const STREAM_HEADER_VERSION: u8 = 1;

/// Header payload size: magic 4, version 1, class 1, publisher session id 8,
/// media kind 1.
pub const STREAM_HEADER_LEN: usize = 15;

/// Relay cap on concurrent downlink streams per receiver: 1 control, 1 audio,
pub const WT_MAX_DOWNLINK_STREAMS: usize = 48;

/// Which pool a downlink stream belongs to. Held as the raw header byte so an
/// unrecognised class from a future relay is still its own distinct key rather
/// than being folded onto an existing one.
pub mod stream_class {
    /// Receiver-scoped traffic: control packets, RTT probe echoes, and
    /// unattributable frames only while under the datagram MTU — the bulk of
    /// those ride [`OVERFLOW`] instead (contract C2).
    pub const CONTROL: u8 = 0x00;
    /// One publisher's one media kind.
    pub const PUBLISHER: u8 = 0x01;
    /// Shared ordered stream for traffic with no publisher key: bulk media the
    /// relay could not attribute, plus any keys past the per-receiver cap.
    pub const OVERFLOW: u8 = 0x02;
    /// Every audio frame this receiver is sent, from every publisher, on one
    /// receiver-scoped reliable stream (#2724, contract addendum A1/A3). Its
    /// header carries publisher session id 0 and media kind `AUDIO` (2).
    pub const AUDIO: u8 = 0x03;
}

/// Identity of one inbound downlink stream, and the unit the client serialises
/// reader hand-over on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StreamKey {
    /// A relay that sent no header. One shared stream carries everything, so
    Legacy,
    /// The v1 header's three fields, verbatim. Never re-derived from
    V1 {
        class: u8,
        publisher_session_id: u64,
        media_kind: u8,
    },
}

/// What the head of an inbound stream turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prelude {
    /// A v1 header. The first `consumed` bytes are the header frame and must
    /// never be emitted upward as a packet.
    Header { key: StreamKey, consumed: usize },
    /// No header: a legacy relay, and `head` already starts at a packet frame.
    Legacy,
    /// Too few bytes buffered to decide yet.
    NeedMore,
}

/// Classify the head of an inbound downlink stream.
///
/// `head` is the unread prefix of the stream's framing buffer. The decision
/// needs at most `4 + STREAM_HEADER_LEN` bytes, so a caller that reads until
/// this stops returning [`Prelude::NeedMore`] cannot buffer without bound.
///
/// A corrupt length is deliberately reported as [`Prelude::Legacy`] rather
/// than handled here, so the one existing frame-drain guard stays the single
/// place that rejects it.
pub fn classify_prelude(head: &[u8]) -> Prelude {
    if head.len() < 4 {
        return Prelude::NeedMore;
    }
    let payload_len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if payload_len != STREAM_HEADER_LEN {
        return Prelude::Legacy;
    }
    let frame_end = 4 + STREAM_HEADER_LEN;
    if head.len() < frame_end {
        return Prelude::NeedMore;
    }
    let payload = &head[4..frame_end];
    if payload[..4] != STREAM_HEADER_MAGIC || payload[4] != STREAM_HEADER_VERSION {
        return Prelude::Legacy;
    }
    let mut session_id = [0u8; 8];
    session_id.copy_from_slice(&payload[6..14]);
    Prelude::Header {
        key: StreamKey::V1 {
            class: payload[5],
            publisher_session_id: u64::from_be_bytes(session_id),
            media_kind: payload[14],
        },
        consumed: frame_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built v1 header frame: the wire bytes, not a call to the parser.
    fn header_frame(version: u8, class: u8, session_id: u64, media_kind: u8) -> Vec<u8> {
        let mut payload = Vec::with_capacity(STREAM_HEADER_LEN);
        payload.extend_from_slice(b"VCDS");
        payload.push(version);
        payload.push(class);
        payload.extend_from_slice(&session_id.to_be_bytes());
        payload.push(media_kind);
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);
        frame
    }

    #[test]
    fn a_v1_header_yields_its_three_fields_verbatim() {
        let frame = header_frame(1, stream_class::PUBLISHER, 0x0102_0304_0506_0708, 3);
        assert_eq!(
            frame.len(),
            19,
            "4-byte length prefix plus a 15-byte payload"
        );
        assert_eq!(
            classify_prelude(&frame),
            Prelude::Header {
                key: StreamKey::V1 {
                    class: stream_class::PUBLISHER,
                    publisher_session_id: 0x0102_0304_0506_0708,
                    media_kind: 3,
                },
                consumed: 19,
            },
            "the session id is big-endian and the media kind is the trailing byte"
        );
    }

    #[test]
    fn the_control_and_overflow_classes_are_each_their_own_key() {
        let control = classify_prelude(&header_frame(1, stream_class::CONTROL, 0, 0));
        let overflow = classify_prelude(&header_frame(1, stream_class::OVERFLOW, 0, 0));
        let publisher = classify_prelude(&header_frame(1, stream_class::PUBLISHER, 0, 0));
        assert_ne!(
            control, overflow,
            "control must not share overflow's reader"
        );
        assert_ne!(control, publisher);
        assert_ne!(overflow, publisher);
    }

    #[test]
    fn an_overflow_header_carries_its_class_verbatim() {
        assert_eq!(
            classify_prelude(&header_frame(1, stream_class::OVERFLOW, 0, 0)),
            Prelude::Header {
                key: StreamKey::V1 {
                    class: stream_class::OVERFLOW,
                    publisher_session_id: 0,
                    media_kind: 0,
                },
                consumed: 19,
            }
        );
    }

    #[test]
    fn one_publisher_gets_a_distinct_key_per_media_kind() {
        let video = classify_prelude(&header_frame(1, stream_class::PUBLISHER, 77, 1));
        let screen = classify_prelude(&header_frame(1, stream_class::PUBLISHER, 77, 3));
        assert_ne!(
            video, screen,
            "camera and screen from one publisher are separate streams and must not \
             serialise against each other"
        );
    }

    #[test]
    fn the_audio_class_parses_on_v1_and_keys_apart_from_every_other_lane() {
        let audio = header_frame(1, stream_class::AUDIO, 0, 2);
        assert_eq!(
            classify_prelude(&audio),
            Prelude::Header {
                key: StreamKey::V1 {
                    class: stream_class::AUDIO,
                    publisher_session_id: 0,
                    media_kind: 2,
                },
                consumed: 19,
            },
            "the audio lane is receiver-scoped: session id 0, media kind AUDIO"
        );

        let audio_key = classify_prelude(&audio);
        for (other, name) in [
            (header_frame(1, stream_class::CONTROL, 0, 0), "control"),
            (header_frame(1, stream_class::OVERFLOW, 0, 0), "overflow"),
            (header_frame(1, stream_class::PUBLISHER, 41, 1), "video"),
            (header_frame(1, stream_class::PUBLISHER, 41, 3), "screen"),
            (header_frame(1, 0x07, 0, 0), "a future class"),
        ] {
            assert_ne!(
                audio_key,
                classify_prelude(&other),
                "audio must not share {name}'s reader: a stalled lane would then \
                 hold audio shut, which is the failure #2724 exists to remove"
            );
        }
    }

    #[test]
    fn the_audio_class_needs_no_version_bump_to_reach_a_shipped_client() {
        assert_eq!(STREAM_HEADER_VERSION, 1);
        assert_eq!(
            DOWNLINK_STREAMS_QUERY, "ds=1",
            "a bump would make every shipped ds=1 client fall back to the single \
             legacy stream, losing a capability it already has"
        );
        assert!(
            matches!(
                classify_prelude(&header_frame(
                    STREAM_HEADER_VERSION,
                    stream_class::AUDIO,
                    0,
                    2
                )),
                Prelude::Header { .. }
            ),
            "class 3 rides the SAME version the shipped client advertises"
        );
    }

    #[test]
    fn an_unrecognised_class_is_its_own_key_rather_than_folded_onto_control() {
        let future = classify_prelude(&header_frame(1, 0x07, 0, 0));
        let control = classify_prelude(&header_frame(1, stream_class::CONTROL, 0, 0));
        assert_ne!(
            future, control,
            "the class byte is carried verbatim, so an unknown pool cannot steal \
             control's reader slot"
        );
    }

    #[test]
    fn a_wrong_magic_or_version_falls_back_to_legacy_instead_of_keying_on_garbage() {
        let mut wrong_magic = header_frame(1, stream_class::PUBLISHER, 5, 1);
        wrong_magic[4] = b'X';
        assert_eq!(
            classify_prelude(&wrong_magic),
            Prelude::Legacy,
            "a 15-byte first frame that is not VCDS is a packet, not a header"
        );

        assert_eq!(
            classify_prelude(&header_frame(2, stream_class::PUBLISHER, 5, 1)),
            Prelude::Legacy,
            "an unparseable version must not be keyed on: a v2 relay never sends v2 \
             to a client that asked for ds=1"
        );

        assert_eq!(
            classify_prelude(&header_frame(0, stream_class::PUBLISHER, 5, 1)),
            Prelude::Legacy
        );
    }

    #[test]
    fn a_first_frame_of_any_other_length_is_a_legacy_packet() {
        let mut frame = 900u32.to_be_bytes().to_vec();
        frame.extend_from_slice(&[0x08; 900]);
        assert_eq!(
            classify_prelude(&frame),
            Prelude::Legacy,
            "the common legacy case decides on the 4-byte length alone"
        );

        let mut short = 14u32.to_be_bytes().to_vec();
        short.extend_from_slice(b"VCDS\x01\x01aaaaaaaa");
        assert_eq!(
            classify_prelude(&short),
            Prelude::Legacy,
            "a truncated header is not a header"
        );
    }

    #[test]
    fn classification_needs_more_bytes_only_below_the_header_frame_size() {
        let frame = header_frame(1, stream_class::PUBLISHER, 9, 1);
        for prefix in 0..4 {
            assert_eq!(
                classify_prelude(&frame[..prefix]),
                Prelude::NeedMore,
                "the length prefix itself is not buffered yet"
            );
        }
        for prefix in 4..frame.len() {
            assert_eq!(
                classify_prelude(&frame[..prefix]),
                Prelude::NeedMore,
                "a header-length frame stays undecided until all {} bytes are in",
                frame.len()
            );
        }
        assert!(matches!(classify_prelude(&frame), Prelude::Header { .. }));
    }

    #[test]
    fn no_protobuf_tag_byte_can_equal_the_magics_first_byte() {
        let magic = STREAM_HEADER_MAGIC[0];
        assert_eq!(magic, 0x56);
        assert_eq!(magic & 0x07, 6, "wire type 6 is not defined by protobuf");
        assert_eq!(magic >> 3, 10, "and PacketWrapper's fields stop at 6");

        for field_number in 1u8..=6 {
            for wire_type in 0u8..=5 {
                assert_ne!(
                    (field_number << 3) | wire_type,
                    magic,
                    "no tag a PacketWrapper can open with may collide with the magic"
                );
            }
        }
    }

    #[test]
    fn the_stream_cap_leaves_room_below_the_browsers_server_initiated_limit() {
        let chrome_usable_server_initiated_uni_streams = 100usize;
        let cap = WT_MAX_DOWNLINK_STREAMS;
        assert!(
            2 * cap <= chrome_usable_server_initiated_uni_streams,
            "a congested receiver sheds its WHOLE map at once, so twice the cap \
             must fit the browser's limit: every replacement needs a fresh id \
             while the reset one still counts until MAX_STREAMS arrives"
        );
        assert_eq!(
            cap, 48,
            "1 control + 1 audio + 1 overflow + 45 publisher keys: #2724 took \
             its lane out of the publisher budget instead of raising the cap, \
             so the peak above stays 96 of 100"
        );
    }

    #[test]
    fn the_capability_parameter_is_a_versioned_query_pair() {
        assert_eq!(DOWNLINK_STREAMS_QUERY, "ds=1");
        let (name, value) = DOWNLINK_STREAMS_QUERY.split_once('=').unwrap();
        assert_eq!(name, "ds");
        assert_eq!(
            value.parse::<u32>().unwrap(),
            u32::from(STREAM_HEADER_VERSION),
            "the advertised version and the header version this client parses are \
             the same number"
        );
    }
}
