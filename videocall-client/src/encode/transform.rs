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
    email: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    if let Err(e) = chunk.copy_to_with_u8_array(&buffer_to_uint8array(buffer)) {
        log::error!("Error copying video chunk: {e:?}");
    }
    let mut media_packet: MediaPacket = MediaPacket {
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        email: email.to_owned(),
        media_type: MediaType::VIDEO.into(),
        timestamp: chunk.timestamp(),
        video_metadata: Some(VideoMetadata {
            sequence,
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
        email: media_packet.email,
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}

pub fn transform_screen_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    if let Err(e) = chunk.copy_to_with_u8_array(&buffer_to_uint8array(buffer)) {
        log::error!("Error copying video chunk: {e:?}");
    }
    let mut media_packet: MediaPacket = MediaPacket {
        email: email.to_owned(),
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        media_type: MediaType::SCREEN.into(),
        timestamp: chunk.timestamp(),
        video_metadata: Some(VideoMetadata {
            sequence,
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
        email: media_packet.email,
        packet_type: PacketType::MEDIA.into(),
        ..Default::default()
    }
}
