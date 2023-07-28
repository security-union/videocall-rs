use crate::crypto::aes::Aes128State;

use super::super::wrappers::{EncodedAudioChunkTypeWrapper, EncodedVideoChunkTypeWrapper};
use protobuf::Message;
use types::protos::{
    media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};
use web_sys::{EncodedAudioChunk, EncodedVideoChunk};

pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: Box<String>,
    aes: Aes128State,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    let mut media_packet: MediaPacket = MediaPacket {
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        email: *email,
        media_type: MediaType::VIDEO.into(),
        timestamp: chunk.timestamp(),
        ..Default::default()
    };
    let mut video_metadata = VideoMetadata::default();
    video_metadata.sequence = sequence;
    media_packet.video_metadata = Some(video_metadata).into();
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    let mut packet: PacketWrapper = PacketWrapper::default();
    packet.data = data;
    packet.email = media_packet.email;
    packet.packet_type = PacketType::MEDIA.into();
    packet
}

pub fn transform_screen_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: Box<String>,
    aes: Aes128State,
) -> PacketWrapper {
    let mut media_packet: MediaPacket = MediaPacket::default();
    media_packet.email = *email;
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    media_packet.data = buffer[0..byte_length].to_vec();
    media_packet.frame_type = EncodedVideoChunkTypeWrapper(chunk.type_()).to_string();
    media_packet.media_type = MediaType::SCREEN.into();
    media_packet.timestamp = chunk.timestamp();
    let mut video_metadata = VideoMetadata::default();
    video_metadata.sequence = sequence;
    media_packet.video_metadata = Some(video_metadata).into();
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    let mut packet: PacketWrapper = PacketWrapper::default();
    packet.data = data;
    packet.email = media_packet.email;
    packet.packet_type = PacketType::MEDIA.into();
    packet
}

pub fn transform_audio_chunk(
    chunk: &EncodedAudioChunk,
    buffer: &mut [u8],
    email: &String,
    sequence: u64,
    aes: Aes128State,
) -> PacketWrapper {
    chunk.copy_to_with_u8_array(buffer);
    let mut media_packet: MediaPacket = MediaPacket::default();
    media_packet.email = email.clone();
    media_packet.media_type = MediaType::AUDIO.into();
    media_packet.data = buffer[0..chunk.byte_length() as usize].to_vec();
    media_packet.frame_type = EncodedAudioChunkTypeWrapper(chunk.type_()).to_string();
    media_packet.timestamp = chunk.timestamp();
    let mut video_metadata = VideoMetadata::default();
    video_metadata.sequence = sequence;
    media_packet.video_metadata = Some(video_metadata).into();
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    let data = media_packet.write_to_bytes().unwrap();
    let data = aes.encrypt(&data).unwrap();
    let mut packet: PacketWrapper = PacketWrapper::default();
    packet.data = data;
    packet.email = media_packet.email;
    packet.packet_type = PacketType::MEDIA.into();
    packet
}
