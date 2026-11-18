// Port target: WebRTC modules/audio_coding/neteq/red_payload_splitter.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: red_payload_splitter_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! RFC 2198 RED payload splitting.

use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::packet::{Packet, PacketList};

/// 4 bytes RED header.
pub const K_RED_HEADER_LENGTH: usize = 4;
/// Reduced size for the last RED header.
pub const K_RED_LAST_HEADER_LENGTH: usize = 1;

/// One parsed RED block header (C++ local `RedHeader`).
struct RedHeader {
    payload_type: u8,
    timestamp: u32,
    payload_length: usize,
}

/// C++ `RedPayloadSplitter`.
#[derive(Debug, Default, Clone)]
pub struct RedPayloadSplitter;

impl RedPayloadSplitter {
    /// A splitter (stateless).
    pub fn new() -> Self {
        Self
    }

    /// Splits every RED packet in `packet_list` into its blocks, in place.
    /// Returns false if any packet was malformed.
    pub fn split_red(&mut self, packet_list: &mut PacketList) -> bool {
        // Too many RED blocks indicates that something is wrong. Clamp it at
        // some reasonable value.
        const K_MAX_RED_BLOCKS: usize = 32;
        let mut ret = true;
        // Index of the C++ iterator `it` into `packet_list`.
        let mut it = 0usize;
        while it < packet_list.len() {
            let red_packet = &packet_list[it];
            debug_assert!(!red_packet.payload.is_empty());
            let red_payload_size = red_packet.payload.len();
            // Offset of the C++ `payload_ptr` into `red_packet.payload`.
            let mut payload_pos = 0usize;
            let mut payload_length = red_payload_size;

            // Read RED headers (according to RFC 2198):
            //
            //    0                   1                   2                   3
            //    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
            //   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
            //   |F|   block PT  |  timestamp offset         |   block length    |
            //   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
            // Last RED header:
            //    0 1 2 3 4 5 6 7
            //   +-+-+-+-+-+-+-+-+
            //   |0|   Block PT  |
            //   +-+-+-+-+-+-+-+-+

            let mut new_headers: Vec<RedHeader> = Vec::new();
            let mut last_block = false;
            let mut sum_length = 0usize;
            while !last_block {
                if payload_length == 0 {
                    log::warn!("SplitRed header too short");
                    return false;
                }
                let payload = &red_packet.payload[payload_pos..];
                // Check the F bit. If F == 0, this was the last block.
                last_block = (payload[0] & 0x80) == 0;
                // Bits 1 through 7 are payload type.
                let payload_type = payload[0] & 0x7F;
                let new_header = if last_block {
                    // No more header data to read.
                    sum_length += K_RED_LAST_HEADER_LENGTH; // Account for RED header size.
                    let header = RedHeader {
                        payload_type,
                        timestamp: red_packet.timestamp,
                        // C++ `size_t` subtraction: wraps on corrupt input and
                        // is caught by the length check below.
                        payload_length: red_payload_size.wrapping_sub(sum_length),
                    };
                    payload_pos += K_RED_LAST_HEADER_LENGTH; // Advance to first payload byte.
                    payload_length -= K_RED_LAST_HEADER_LENGTH;
                    header
                } else {
                    if payload_length < K_RED_HEADER_LENGTH {
                        log::warn!("SplitRed header too short");
                        return false;
                    }
                    // Bits 8 through 21 are timestamp offset.
                    let timestamp_offset =
                        ((payload[1] as i32) << 6) + ((payload[2] as i32 & 0xFC) >> 2);
                    // Bits 22 through 31 are payload length.
                    let block_length = (((payload[2] as usize) & 0x03) << 8) + payload[3] as usize;
                    let header = RedHeader {
                        payload_type,
                        timestamp: red_packet.timestamp.wrapping_sub(timestamp_offset as u32),
                        payload_length: block_length,
                    };

                    sum_length += block_length;
                    sum_length += K_RED_HEADER_LENGTH; // Account for RED header size.

                    payload_pos += K_RED_HEADER_LENGTH; // Advance to next RED header.
                    payload_length -= K_RED_HEADER_LENGTH;
                    header
                };
                // Store in new list of packets.
                if new_header.payload_length > 0 {
                    new_headers.push(new_header);
                }
            }

            let mut new_packets = PacketList::new();
            if new_headers.len() <= K_MAX_RED_BLOCKS {
                // Populate the new packets with payload data.
                // `payload_pos` now points at the first payload byte.
                for (i, new_header) in new_headers.iter().enumerate() {
                    let block_length = new_header.payload_length;
                    if block_length > red_payload_size - payload_pos {
                        // The block lengths in the RED headers do not match the
                        // overall packet length. Something is corrupt. Discard
                        // this and the remaining payloads from this packet.
                        log::warn!("SplitRed length mismatch");
                        ret = false;
                        break;
                    }

                    let mut new_packet = Packet::new();
                    new_packet.timestamp = new_header.timestamp;
                    new_packet.payload_type = new_header.payload_type;
                    new_packet.sequence_number = red_packet.sequence_number;
                    new_packet.priority.red_level = ((new_headers.len() - 1) - i) as i32;
                    new_packet.payload =
                        red_packet.payload[payload_pos..payload_pos + block_length].to_vec();
                    new_packets.push_front(new_packet);
                    payload_pos += block_length;
                }
            } else {
                log::warn!("SplitRed too many blocks: {}", new_headers.len());
                ret = false;
            }
            // Insert the new packets into the original list, before `it`
            // (C++ `splice`), then remove `it`; `it` ends up on the next
            // original packet.
            let num_new_packets = new_packets.len();
            packet_list.remove(it);
            for (offset, new_packet) in new_packets.into_iter().enumerate() {
                packet_list.insert(it + offset, new_packet);
            }
            it += num_new_packets;
        }
        ret
    }

    /// Drops packets whose payload type is not the first non-DTMF,
    /// non-CN type found in the list.
    pub fn check_red_payloads(
        &mut self,
        packet_list: &mut PacketList,
        decoder_database: &DecoderDatabase,
    ) {
        let mut main_payload_type: i32 = -1;
        packet_list.retain(|packet| {
            let this_payload_type = packet.payload_type;
            if decoder_database.is_red(this_payload_type) {
                return false;
            }
            if !decoder_database.is_dtmf(this_payload_type)
                && !decoder_database.is_comfort_noise(this_payload_type)
            {
                if main_payload_type == -1 {
                    // This is the first packet in the list which is non-DTMF
                    // non-CNG.
                    main_payload_type = this_payload_type as i32;
                } else if this_payload_type as i32 != main_payload_type {
                    // We do not allow redundant payloads of a different type.
                    return false;
                }
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    //! Port of red_payload_splitter_unittest.cc.

    use std::sync::Arc;

    use super::*;
    use crate::api::audio_decoder::{AudioDecoder, AudioDecoderFactory};
    use crate::api::audio_format::SdpAudioFormat;
    use crate::neteq::packet::Priority;

    const K_RED_PAYLOAD_TYPE: u8 = 100;
    const K_PAYLOAD_LENGTH: usize = 10;
    const K_SEQUENCE_NUMBER: u16 = 0;
    const K_BASE_TIMESTAMP: u32 = 0x12345678;
    // Max length described by the 10-bit RED block length.
    const _: () = assert!(K_PAYLOAD_LENGTH <= 1023);

    /// C++ `MockAudioDecoderFactory`; never asked for a decoder here.
    struct NoDecoderFactory;

    impl AudioDecoderFactory for NoDecoderFactory {
        fn make_decoder(&self, _format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
            None
        }
    }

    /// A possible Opus packet that contains FEC (20 ms frame):
    /// TOC 0x08, then a first data byte with the LBRR bit set.
    fn create_opus_fec_payload(payload: &mut [u8], payload_value: u8) {
        if payload.len() < 2 {
            return;
        }
        payload[0] = 0x08;
        payload[1] = 0x40;
        payload[2..].fill(payload_value);
    }

    /// Creates a RED packet with `payload_types.len()` payloads. Each
    /// redundant payload is `timestamp_offset` samples "behind" the previous
    /// payload.
    fn create_red_payload(
        payload_types: &[u8],
        timestamp_offset: i32,
        embed_opus_fec: bool,
    ) -> Packet {
        let num_payloads = payload_types.len();
        let mut packet = Packet::new();
        packet.payload_type = K_RED_PAYLOAD_TYPE;
        packet.timestamp = K_BASE_TIMESTAMP;
        packet.sequence_number = K_SEQUENCE_NUMBER;
        let mut payload = Vec::with_capacity(
            (K_PAYLOAD_LENGTH + 1) + (num_payloads - 1) * (K_PAYLOAD_LENGTH + K_RED_HEADER_LENGTH),
        );
        for (i, &payload_type) in payload_types.iter().enumerate() {
            // Write the RED headers.
            if i == num_payloads - 1 {
                // Special case for last payload.
                payload.push(payload_type & 0x7F); // F = 0;
                break;
            }
            // Not the last block; set F = 1.
            payload.push((payload_type & 0x7F) | 0x80);
            let this_offset = (num_payloads - i - 1) as i32 * timestamp_offset;
            payload.push((this_offset >> 6) as u8);
            payload.push((((this_offset & 0x3F) << 2) as usize | (K_PAYLOAD_LENGTH >> 8)) as u8);
            payload.push((K_PAYLOAD_LENGTH & 0xFF) as u8);
        }
        for i in 0..num_payloads {
            // Write `i` to all bytes in each payload.
            let mut block = vec![i as u8; K_PAYLOAD_LENGTH];
            if embed_opus_fec {
                create_opus_fec_payload(&mut block, i as u8);
            }
            payload.extend_from_slice(&block);
        }
        packet.payload = payload;
        packet
    }

    /// Creates a packet with all payload bytes set to `payload_value`.
    fn create_packet(payload_type: u8, payload_length: usize, payload_value: u8) -> Packet {
        let mut packet = Packet::new();
        packet.payload_type = payload_type;
        packet.timestamp = K_BASE_TIMESTAMP;
        packet.sequence_number = K_SEQUENCE_NUMBER;
        packet.payload = vec![payload_value; payload_length];
        packet
    }

    /// Checks that `packet` has the given attributes.
    fn verify_packet(
        packet: &Packet,
        payload_length: usize,
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        payload_value: u8,
        priority: Priority,
    ) {
        assert_eq!(payload_length, packet.payload.len());
        assert_eq!(payload_type, packet.payload_type);
        assert_eq!(sequence_number, packet.sequence_number);
        assert_eq!(timestamp, packet.timestamp);
        assert_eq!(priority, packet.priority);
        assert!(!packet.payload.is_empty());
        assert!(packet.payload.iter().all(|&byte| byte == payload_value));
    }

    fn primary(is_primary: bool) -> Priority {
        Priority::new(0, if is_primary { 0 } else { 1 })
    }

    #[test]
    fn create_and_destroy() {
        let _splitter = RedPayloadSplitter::new();
    }

    /// Packet A is split into A1 and A2.
    #[test]
    fn one_packet_two_payloads() {
        let payload_types = [0u8, 0];
        const K_TIMESTAMP_OFFSET: i32 = 160;
        let mut packet_list = PacketList::new();
        packet_list.push_back(create_red_payload(
            &payload_types,
            K_TIMESTAMP_OFFSET,
            false,
        ));
        let mut splitter = RedPayloadSplitter::new();
        assert!(splitter.split_red(&mut packet_list));
        assert_eq!(2, packet_list.len());
        // Check first packet. The first in list should always be the primary
        // payload.
        verify_packet(
            &packet_list[0],
            K_PAYLOAD_LENGTH,
            payload_types[1],
            K_SEQUENCE_NUMBER,
            K_BASE_TIMESTAMP,
            1,
            primary(true),
        );
        // Check second packet.
        verify_packet(
            &packet_list[1],
            K_PAYLOAD_LENGTH,
            payload_types[0],
            K_SEQUENCE_NUMBER,
            K_BASE_TIMESTAMP - K_TIMESTAMP_OFFSET as u32,
            0,
            primary(false),
        );
    }

    /// Packets A and B are not split at all. Only the RED header in each
    /// packet is removed.
    #[test]
    fn two_packets_one_payload() {
        let payload_types = [0u8];
        const K_TIMESTAMP_OFFSET: i32 = 160;
        let mut packet_list = PacketList::new();
        packet_list.push_back(create_red_payload(
            &payload_types,
            K_TIMESTAMP_OFFSET,
            false,
        ));
        let mut packet = create_red_payload(&payload_types, K_TIMESTAMP_OFFSET, false);
        // Manually change timestamp and sequence number of second packet.
        packet.timestamp += K_TIMESTAMP_OFFSET as u32;
        packet.sequence_number += 1;
        packet_list.push_back(packet);
        let mut splitter = RedPayloadSplitter::new();
        assert!(splitter.split_red(&mut packet_list));
        assert_eq!(2, packet_list.len());
        verify_packet(
            &packet_list[0],
            K_PAYLOAD_LENGTH,
            payload_types[0],
            K_SEQUENCE_NUMBER,
            K_BASE_TIMESTAMP,
            0,
            primary(true),
        );
        verify_packet(
            &packet_list[1],
            K_PAYLOAD_LENGTH,
            payload_types[0],
            K_SEQUENCE_NUMBER + 1,
            K_BASE_TIMESTAMP + K_TIMESTAMP_OFFSET as u32,
            0,
            primary(true),
        );
    }

    /// Packets A and B are split into packets A1, A2, A3, B1, B2, B3, with
    /// attributes as follows:
    ///
    /// ```text
    ///                  A1*   A2    A3    B1*   B2    B3
    /// Payload type     0     1     2     0     1     2
    /// Timestamp        b     b-o   b-2o  b+o   b     b-o
    /// Sequence number  0     0     0     1     1     1
    /// ```
    ///
    /// b = kBaseTimestamp, o = kTimestampOffset, * = primary.
    #[test]
    fn two_packets_three_payloads() {
        let payload_types = [2u8, 1, 0]; // Primary is the last one.
        const K_TIMESTAMP_OFFSET: u32 = 160;
        let mut packet_list = PacketList::new();
        packet_list.push_back(create_red_payload(
            &payload_types,
            K_TIMESTAMP_OFFSET as i32,
            false,
        ));
        let mut packet = create_red_payload(&payload_types, K_TIMESTAMP_OFFSET as i32, false);
        packet.timestamp += K_TIMESTAMP_OFFSET;
        packet.sequence_number += 1;
        packet_list.push_back(packet);
        let mut splitter = RedPayloadSplitter::new();
        assert!(splitter.split_red(&mut packet_list));
        assert_eq!(6, packet_list.len());
        let expected = [
            (payload_types[2], K_SEQUENCE_NUMBER, K_BASE_TIMESTAMP, 2, 0),
            (
                payload_types[1],
                K_SEQUENCE_NUMBER,
                K_BASE_TIMESTAMP - K_TIMESTAMP_OFFSET,
                1,
                1,
            ),
            (
                payload_types[0],
                K_SEQUENCE_NUMBER,
                K_BASE_TIMESTAMP - 2 * K_TIMESTAMP_OFFSET,
                0,
                2,
            ),
            (
                payload_types[2],
                K_SEQUENCE_NUMBER + 1,
                K_BASE_TIMESTAMP + K_TIMESTAMP_OFFSET,
                2,
                0,
            ),
            (
                payload_types[1],
                K_SEQUENCE_NUMBER + 1,
                K_BASE_TIMESTAMP,
                1,
                1,
            ),
            (
                payload_types[0],
                K_SEQUENCE_NUMBER + 1,
                K_BASE_TIMESTAMP - K_TIMESTAMP_OFFSET,
                0,
                2,
            ),
        ];
        for (packet, &(payload_type, sequence_number, timestamp, value, red_level)) in
            packet_list.iter().zip(expected.iter())
        {
            verify_packet(
                packet,
                K_PAYLOAD_LENGTH,
                payload_type,
                sequence_number,
                timestamp,
                value,
                Priority::new(0, red_level),
            );
        }
    }

    /// Creates a list with 4 packets with these payload types:
    /// 0 = CNGnb, 1 = PCMu, 2 = DTMF (AVT), 3 = PCMa.
    /// `check_red_payloads` must discard the PCMa packet, since it is a
    /// non-CNG, non-DTMF payload of another type than the first speech
    /// payload found in the list (which is PCMu).
    #[test]
    fn check_red_payloads() {
        let mut packet_list = PacketList::new();
        for i in 0..=3u8 {
            // Create packet with payload type `i`, payload length 10 bytes,
            // all 0.
            packet_list.push_back(create_packet(i, 10, 0));
        }

        // Use a real DecoderDatabase object here instead of a mock, since it
        // is easier to just register the payload types and let the actual
        // implementation do its job.
        let mut decoder_database = DecoderDatabase::new(Arc::new(NoDecoderFactory));
        decoder_database.register_payload(0, &SdpAudioFormat::new("cn", 8000, 1));
        decoder_database.register_payload(1, &SdpAudioFormat::new("pcmu", 8000, 1));
        decoder_database.register_payload(2, &SdpAudioFormat::new("telephone-event", 8000, 1));
        decoder_database.register_payload(1, &SdpAudioFormat::new("pcma", 8000, 1));

        let mut splitter = RedPayloadSplitter::new();
        splitter.check_red_payloads(&mut packet_list, &decoder_database);

        assert_eq!(3, packet_list.len()); // Should have dropped the last packet.
                                          // Verify packets. The loop verifies that payload types 0, 1, and 2 are
                                          // in the list.
        for (i, packet) in packet_list.iter().enumerate() {
            verify_packet(
                packet,
                10,
                i as u8,
                K_SEQUENCE_NUMBER,
                K_BASE_TIMESTAMP,
                0,
                primary(true),
            );
        }
    }

    /// A RED packet whose payloads also have the RED payload type (nested
    /// RED) is not supported; the splitter discards all packets.
    #[test]
    fn check_red_payloads_recursive_red() {
        let mut packet_list = PacketList::new();
        for _ in 0..=3 {
            // Create packet with RED payload type, payload length 10 bytes,
            // all 0.
            packet_list.push_back(create_packet(K_RED_PAYLOAD_TYPE, 10, 0));
        }

        let mut decoder_database = DecoderDatabase::new(Arc::new(NoDecoderFactory));
        decoder_database.register_payload(
            K_RED_PAYLOAD_TYPE as i32,
            &SdpAudioFormat::new("red", 8000, 1),
        );

        let mut splitter = RedPayloadSplitter::new();
        splitter.check_red_payloads(&mut packet_list, &decoder_database);

        assert!(packet_list.is_empty()); // Should have dropped all packets.
    }

    /// Packet A is split into A1, A2 and A3. But the length parameter is off,
    /// so the last payloads should be discarded.
    #[test]
    fn wrong_payload_length() {
        let payload_types = [0u8, 0, 0];
        const K_TIMESTAMP_OFFSET: u32 = 160;
        let mut packet_list = PacketList::new();
        let mut packet = create_red_payload(&payload_types, K_TIMESTAMP_OFFSET as i32, false);
        // Manually tamper with the payload length of the packet. This is one
        // byte too short for the second payload (out of three). We expect only
        // the first payload to be returned.
        let new_len = packet.payload.len() - (K_PAYLOAD_LENGTH + 1);
        packet.payload.truncate(new_len);
        packet_list.push_back(packet);
        let mut splitter = RedPayloadSplitter::new();
        assert!(!splitter.split_red(&mut packet_list));
        assert_eq!(1, packet_list.len());
        verify_packet(
            &packet_list[0],
            K_PAYLOAD_LENGTH,
            payload_types[0],
            K_SEQUENCE_NUMBER,
            K_BASE_TIMESTAMP - 2 * K_TIMESTAMP_OFFSET,
            0,
            Priority::new(0, 2),
        );
    }

    /// Packets too short to contain a RED header are rejected.
    #[test]
    fn rejects_incomplete_headers() {
        let mut splitter = RedPayloadSplitter::new();

        let payload_types = [0u8, 0];
        const K_TIMESTAMP_OFFSET: i32 = 160;

        let mut packet_list = PacketList::new();

        // Truncate the packet such that the first block can not be parsed.
        packet_list.push_back(create_red_payload(
            &payload_types,
            K_TIMESTAMP_OFFSET,
            false,
        ));
        packet_list[0].payload.truncate(4);
        assert!(!splitter.split_red(&mut packet_list));
        assert!(!packet_list.is_empty());

        // Truncate the packet such that the first block can not be parsed.
        packet_list[0].payload.truncate(3);
        assert!(!splitter.split_red(&mut packet_list));
        assert!(!packet_list.is_empty());
    }

    /// RED blocks keep their bytes verbatim (the C++ helper's Opus FEC
    /// variant): the TOC and LBRR bytes survive the split.
    #[test]
    fn opus_fec_payloads_survive_split() {
        let payload_types = [0u8, 0];
        let mut packet_list = PacketList::new();
        packet_list.push_back(create_red_payload(&payload_types, 960, true));
        let mut splitter = RedPayloadSplitter::new();
        assert!(splitter.split_red(&mut packet_list));
        assert_eq!(2, packet_list.len());
        for (packet, value) in packet_list.iter().zip([1u8, 0]) {
            assert_eq!(&packet.payload[..2], &[0x08, 0x40]);
            assert!(packet.payload[2..].iter().all(|&byte| byte == value));
        }
        assert_eq!(K_BASE_TIMESTAMP - 960, packet_list[1].timestamp);
    }
}
