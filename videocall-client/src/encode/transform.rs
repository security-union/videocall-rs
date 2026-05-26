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
    media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};
use web_sys::EncodedVideoChunk;

pub fn buffer_to_uint8array(buf: &mut [u8]) -> Uint8Array {
    // Convert &mut [u8] to a Uint8Array
    unsafe { Uint8Array::view_mut_raw(buf.as_mut_ptr(), buf.len()) }
}

pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    user_id: &str,
    aes: Rc<Aes128State>,
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
        video_metadata: Some(VideoMetadata {
            sequence,
            codec: get_video_codec().into(),
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
        ..Default::default()
    }
}
