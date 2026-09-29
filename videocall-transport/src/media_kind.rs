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

//! Read `PacketWrapper.media_kind` without parsing the packet.

/// `PacketWrapper.MediaKind` (`protobuf/types/packet_wrapper.proto`).
pub mod kind {
    pub const UNSPECIFIED: u8 = 0;
    pub const VIDEO: u8 = 1;
    pub const AUDIO: u8 = 2;
    pub const SCREEN: u8 = 3;
}

const MEDIA_KIND_FIELD: u64 = 6;

const WIRE_VARINT: u64 = 0;
const WIRE_I64: u64 = 1;
const WIRE_LEN: u64 = 2;
const WIRE_I32: u64 = 5;

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

pub fn peek_media_kind(bytes: &[u8]) -> Option<u8> {
    let mut at = 0usize;
    while at < bytes.len() {
        let tag = read_varint(bytes, &mut at)?;
        let field = tag >> 3;
        let wire = tag & 0x7;
        if field == MEDIA_KIND_FIELD && wire == WIRE_VARINT {
            return read_varint(bytes, &mut at).map(|v| v as u8);
        }
        match wire {
            WIRE_VARINT => {
                read_varint(bytes, &mut at)?;
            }
            WIRE_I64 => at = at.checked_add(8)?,
            WIRE_LEN => {
                let len = read_varint(bytes, &mut at)? as usize;
                at = at.checked_add(len)?;
            }
            WIRE_I32 => at = at.checked_add(4)?,
            _ => return None,
        }
        if at > bytes.len() {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    fn round_trip(kind: MediaKind, payload_len: usize, session_id: u64) {
        let packet = PacketWrapper {
            packet_type: videocall_types::protos::packet_wrapper::packet_wrapper::PacketType::MEDIA
                .into(),
            user_id: b"someone@videocall.rs".to_vec(),
            data: vec![0xAB; payload_len],
            session_id,
            simulcast_layer_id: 2,
            media_kind: kind.into(),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let from_parser = parsed.media_kind.enum_value_or_default() as u8;
        let scanned = peek_media_kind(&bytes);
        if from_parser == kind::UNSPECIFIED {
            assert_eq!(
                crate::worker_proto::shed_tier(scanned),
                crate::worker_proto::shed_tier(Some(from_parser)),
                "an unset kind must reach the same verdict either way"
            );
        } else {
            assert_eq!(
                scanned,
                Some(from_parser),
                "the scan must agree with the production parser for {kind:?}"
            );
        }
    }

    #[test]
    fn the_scan_agrees_with_the_real_parser_on_every_kind() {
        for kind in [
            MediaKind::VIDEO,
            MediaKind::AUDIO,
            MediaKind::SCREEN,
            MediaKind::MEDIA_KIND_UNSPECIFIED,
        ] {
            for payload_len in [0usize, 1, 110, 1400, 200_000] {
                round_trip(kind, payload_len, 0x0123_4567_89AB_CDEF);
            }
        }
    }

    #[test]
    fn a_packet_that_sets_no_media_kind_reads_as_absent() {
        let mut packet = PacketWrapper::new();
        packet.data = vec![7u8; 64];
        let bytes = packet.write_to_bytes().unwrap();
        assert_eq!(
            PacketWrapper::parse_from_bytes(&bytes).unwrap().data.len(),
            64
        );
        assert_eq!(peek_media_kind(&bytes), None);
    }

    #[test]
    fn a_payload_that_looks_like_a_tag_is_skipped_not_scanned_into() {
        let packet = PacketWrapper {
            data: vec![0x30, 0x01, 0x30, 0x01],
            media_kind: MediaKind::AUDIO.into(),
            ..Default::default()
        };
        let bytes = packet.write_to_bytes().unwrap();
        assert_eq!(
            peek_media_kind(&bytes),
            Some(kind::AUDIO),
            "a VIDEO-looking tag inside the payload must not win over the real field"
        );
    }

    #[test]
    fn truncated_and_malformed_bytes_fail_open() {
        assert_eq!(peek_media_kind(&[]), None);
        assert_eq!(peek_media_kind(&[0x1a, 0x7f, 0x00]), None);
        assert_eq!(peek_media_kind(&[0xff; 12]), None);
        assert_eq!(peek_media_kind(&[0x0b, 0x00]), None);
    }
}
