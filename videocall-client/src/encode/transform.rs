use super::super::wrappers::{EncodedAudioChunkTypeWrapper, EncodedVideoChunkTypeWrapper};
use crate::crypto::aes::Aes128State;
use protobuf::Message;
use std::rc::Rc;
use types::protos::{
    media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};
use web_sys::{EncodedAudioChunk, EncodedVideoChunk};

pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: &str,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
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
    chunk.copy_to_with_u8_array(buffer);
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

pub fn transform_audio_chunk(
    chunk: &EncodedAudioChunk,
    buffer: &mut [u8],
    email: &str,
    sequence: u64,
    aes: Rc<Aes128State>,
) -> PacketWrapper {
    chunk.copy_to_with_u8_array(buffer);
    let mut media_packet: MediaPacket = MediaPacket {
        email: email.to_owned(),
        media_type: MediaType::AUDIO.into(),
        data: buffer[0..chunk.byte_length() as usize].to_vec(),
        frame_type: EncodedAudioChunkTypeWrapper(chunk.type_()).to_string(),
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
