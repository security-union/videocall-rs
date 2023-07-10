use super::super::wrappers::{EncodedAudioChunkTypeWrapper, EncodedVideoChunkTypeWrapper};
use types::protos::media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata};
use web_sys::{EncodedAudioChunk, EncodedVideoChunk};

pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: Box<String>,
) -> MediaPacket {
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
    media_packet
}

pub fn transform_screen_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: Box<String>,
) -> MediaPacket {
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
    media_packet
}

pub fn transform_audio_chunk(
    chunk: &EncodedAudioChunk,
    buffer: &mut [u8],
    email: &String,
    sequence: u64,
) -> MediaPacket {
    chunk.copy_to_with_u8_array(buffer);
    let mut packet: MediaPacket = MediaPacket::default();
    packet.email = email.clone();
    packet.media_type = MediaType::AUDIO.into();
    packet.data = buffer[0..chunk.byte_length() as usize].to_vec();
    packet.frame_type = EncodedAudioChunkTypeWrapper(chunk.type_()).to_string();
    packet.timestamp = chunk.timestamp();
    let mut video_metadata = VideoMetadata::default();
    video_metadata.sequence = sequence;
    packet.video_metadata = Some(video_metadata).into();
    if let Some(duration0) = chunk.duration() {
        packet.duration = duration0;
    }
    packet
}
