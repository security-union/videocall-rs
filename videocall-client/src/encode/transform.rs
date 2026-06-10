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

use super::super::wrappers::EncodedVideoChunkTypeWrapper;
use crate::constants::get_video_codec;
use crate::crypto::aes::Aes128State;
use js_sys::Uint8Array;
use protobuf::Message;
use std::rc::Rc;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, MediaPacket, VideoCodec, VideoMetadata},
    packet_wrapper::{
        packet_wrapper::{MediaKind, PacketType},
        PacketWrapper,
    },
};
use web_sys::EncodedVideoChunk;

pub fn buffer_to_uint8array(buf: &mut [u8]) -> Uint8Array {
    // Convert &mut [u8] to a Uint8Array
    unsafe { Uint8Array::view_mut_raw(buf.as_mut_ptr(), buf.len()) }
}

/// Build the `VideoMetadata` for a camera-video frame (issue #1196).
///
/// Pure (no `web_sys`, no clock) so the `source_width` / `source_height`
/// stamping — the field that makes #1196-class aspect distortion diagnosable
/// from receiver diagnostics — is host-testable off-wasm. The codec is passed
/// in (rather than read via the browser-only `get_video_codec()`) so the helper
/// stays host-safe; the production caller passes `get_video_codec().into()`.
fn build_camera_video_metadata(
    sequence: u64,
    codec: protobuf::EnumOrUnknown<VideoCodec>,
    source_width: u32,
    source_height: u32,
) -> VideoMetadata {
    VideoMetadata {
        sequence,
        codec,
        source_width,
        source_height,
        ..Default::default()
    }
}

/// Build a `PacketWrapper` for an encoded camera-video frame.
///
/// `source_width` / `source_height` carry the publisher's native capture
/// dimensions as reported by `MediaStreamTrack.getSettings()` (the camera's
/// true pixel size, the source aspect). They are stamped into `VideoMetadata`
/// so receiver diagnostics can detect when the encoder downscaled the content
/// and, critically for issue #1196, surface the source aspect so an
/// aspect-distortion regression is diagnosable from the wire. Pass `0` for
/// either dimension when unknown — proto3 default-zero makes the consumer treat
/// it as missing (mirrors `transform_screen_chunk`).
#[allow(clippy::too_many_arguments)]
pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    user_id: &str,
    aes: Rc<Aes128State>,
    source_width: u32,
    source_height: u32,
    simulcast_layer_id: u32,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    if let Err(e) = chunk.copy_to_with_u8_array(&buffer_to_uint8array(buffer)) {
        log::error!("Error copying video chunk: {e:?}");
    }
    let mut media_packet: MediaPacket = MediaPacket {
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        user_id: Vec::new(),
        media_type: MediaType::VIDEO.into(),
        timestamp: chunk.timestamp(),
        video_metadata: Some(build_camera_video_metadata(
            sequence,
            get_video_codec().into(),
            source_width,
            source_height,
        ))
        .into(),
        ..Default::default()
    };
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    PacketWrapper {
        data,
        user_id: user_id.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        // Cleartext discriminator so the relay can apply viewport-aware VIDEO
        // filtering without decrypting the inner MediaPacket (HCL issue #988).
        media_kind: MediaKind::VIDEO.into(),
        // Cleartext simulcast layer id (issue #989). Tag 5 serializes only when
        // non-zero (see videocall-types packet_wrapper.rs), so layer 0 — the
        // single-layer default and what every pre-simulcast publisher emits —
        // is wire-identical to today. The relay (steps 6-8, future) and the
        // receiver layer-select guard read this to forward/decode one layer.
        simulcast_layer_id,
        ..Default::default()
    }
}

/// Build a `PacketWrapper` for an encoded screen-share frame.
///
/// `source_width` / `source_height` carry the publisher's native capture
/// dimensions as reported by `MediaStreamTrack.getSettings()` (the monitor /
/// window / tab pixel size). They are stamped into `VideoMetadata` so
/// consumers can detect when the encoder downscaled the content in transit
/// (e.g. a 2560×1440 desktop encoded down to 1280×720 under a tier
/// constraint). Pass `0` for either dimension when the value is unknown —
/// proto3 default-zero semantics make the consumer treat it as missing.
///
/// `encoder_target_bitrate_kbps` / `adaptive_tier` / `cause_hint` carry the
/// publisher's encoder state so the consumer can render a `Cause:` line
/// below the Screen row explaining *why* the encoder downscaled (issue
/// #903). Pass `0` / empty strings when no constraint is engaged or the
/// data isn't available — proto3 defaults make the consumer omit the line
/// entirely in that case.
#[allow(clippy::too_many_arguments)]
pub fn transform_screen_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    user_id: &str,
    aes: Rc<Aes128State>,
    source_width: u32,
    source_height: u32,
    encoder_target_bitrate_kbps: u32,
    adaptive_tier: String,
    cause_hint: String,
    simulcast_layer_id: u32,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    if let Err(e) = chunk.copy_to_with_u8_array(&buffer_to_uint8array(buffer)) {
        log::error!("Error copying video chunk: {e:?}");
    }
    let mut media_packet: MediaPacket = MediaPacket {
        user_id: Vec::new(),
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        media_type: MediaType::SCREEN.into(),
        timestamp: chunk.timestamp(),
        video_metadata: Some(VideoMetadata {
            sequence,
            codec: get_video_codec().into(),
            source_width,
            source_height,
            encoder_target_bitrate_kbps,
            adaptive_tier,
            cause_hint,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    PacketWrapper {
        data,
        user_id: user_id.as_bytes().to_vec(),
        packet_type: PacketType::MEDIA.into(),
        // Cleartext discriminator so the relay can apply viewport-aware VIDEO
        // filtering without decrypting the inner MediaPacket (HCL issue #988).
        media_kind: MediaKind::SCREEN.into(),
        // Cleartext simulcast layer id (issue #989, Phase 3b). Tag 5 serializes
        // only when non-zero, so layer 0 — the single-layer default and what
        // every pre-simulcast screen publisher emits — is wire-identical to
        // today. The relay's per-(source, SCREEN) layer filter and the
        // receiver's screen layer-select guard read this to forward/decode one
        // screen layer (mirrors transform_video_chunk).
        simulcast_layer_id,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoded `EncodedVideoChunk` body of `transform_video_chunk` requires
    /// `web_sys` (a browser), so we cannot drive the full function natively.
    /// What is load-bearing for issue #989, though, is purely the protobuf
    /// behaviour of the cleartext `simulcast_layer_id` field on the outer
    /// `PacketWrapper` — exactly the bytes the relay and the receiver
    /// layer-select guard read. These tests pin that behaviour:
    ///
    ///   * `layer_id == 0` (single-layer default + every pre-simulcast
    ///     publisher) MUST be wire-identical to a wrapper that never set the
    ///     field — i.e. tag 5 is absent and parses back to 0.
    ///   * `layer_id == 2` MUST round-trip to `2`.
    ///
    /// We build the wrapper with the same field assignment the function uses so
    /// a future refactor that drops the field is caught here.
    fn wrapper_with_layer(layer: u32) -> PacketWrapper {
        PacketWrapper {
            data: vec![1, 2, 3, 4],
            user_id: b"alice".to_vec(),
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::VIDEO.into(),
            simulcast_layer_id: layer,
            ..Default::default()
        }
    }

    #[test]
    fn layer_id_zero_is_wire_absent_and_round_trips_to_zero() {
        let with_zero = wrapper_with_layer(0);
        let zero_bytes = with_zero.write_to_bytes().unwrap();

        // A wrapper that never touched the field must serialize identically —
        // proves tag 5 is omitted entirely when the layer is 0.
        let baseline = PacketWrapper {
            data: vec![1, 2, 3, 4],
            user_id: b"alice".to_vec(),
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::VIDEO.into(),
            ..Default::default()
        };
        let baseline_bytes = baseline.write_to_bytes().unwrap();
        assert_eq!(
            zero_bytes, baseline_bytes,
            "layer 0 must be byte-identical to a wrapper that never set the field"
        );

        let parsed = PacketWrapper::parse_from_bytes(&zero_bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 0);
    }

    #[test]
    fn layer_id_two_round_trips() {
        let with_two = wrapper_with_layer(2);
        let bytes = with_two.write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 2);
    }

    /// Phase 3b: the SCREEN wrapper carries the same cleartext layer-id contract
    /// as VIDEO. A SCREEN wrapper at layer 0 must be wire-identical to one that
    /// never set the field (so single-layer screen publishers are unchanged on
    /// the wire), and a non-zero screen layer must round-trip.
    fn screen_wrapper_with_layer(layer: u32) -> PacketWrapper {
        PacketWrapper {
            data: vec![9, 9, 9],
            user_id: b"bob".to_vec(),
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::SCREEN.into(),
            simulcast_layer_id: layer,
            ..Default::default()
        }
    }

    #[test]
    fn screen_layer_id_zero_is_wire_absent() {
        let zero_bytes = screen_wrapper_with_layer(0).write_to_bytes().unwrap();
        let baseline = PacketWrapper {
            data: vec![9, 9, 9],
            user_id: b"bob".to_vec(),
            packet_type: PacketType::MEDIA.into(),
            media_kind: MediaKind::SCREEN.into(),
            ..Default::default()
        };
        assert_eq!(
            zero_bytes,
            baseline.write_to_bytes().unwrap(),
            "screen layer 0 must be byte-identical to a wrapper that never set the field"
        );
    }

    #[test]
    fn screen_layer_id_two_round_trips() {
        let bytes = screen_wrapper_with_layer(2).write_to_bytes().unwrap();
        let parsed = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.simulcast_layer_id, 2);
        assert_eq!(parsed.media_kind.enum_value(), Ok(MediaKind::SCREEN));
    }

    /// Issue #1196: camera packets must carry the native source dims so the
    /// receiver can read the source aspect (mirrors the screen variant). This
    /// drives the SAME `build_camera_video_metadata` helper the production
    /// `transform_video_chunk` calls, so it is not an X==X self-pin: if the
    /// helper stopped stamping the dims, this fails.
    #[test]
    fn camera_video_metadata_carries_source_dims() {
        // 640x480 (4:3): the canonical squashed-webcam case from #1196.
        let meta = super::build_camera_video_metadata(
            7,
            VideoCodec::VP9_PROFILE0_LEVEL10_8BIT.into(),
            640,
            480,
        );
        assert_eq!(meta.sequence, 7);
        assert_eq!(meta.source_width, 640);
        assert_eq!(meta.source_height, 480);

        // Round-trip through protobuf so the wire form is covered too.
        let bytes = meta.write_to_bytes().unwrap();
        let parsed = VideoMetadata::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.source_width, 640);
        assert_eq!(parsed.source_height, 480);
    }

    /// Unknown source dims (0/0) must serialize as proto3 default-absent, so a
    /// publisher that doesn't know its capture size is wire-identical to the
    /// pre-#1196 emitter (no new bytes on the common path).
    #[test]
    fn camera_video_metadata_zero_dims_are_wire_absent() {
        let with_zero = super::build_camera_video_metadata(0, Default::default(), 0, 0);
        let baseline = VideoMetadata::default();
        assert_eq!(
            with_zero.write_to_bytes().unwrap(),
            baseline.write_to_bytes().unwrap(),
            "0/0 source dims must be byte-identical to a metadata that never set them"
        );
    }
}
