use protobuf::Message;
use std::fmt;
use types::protos::media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata};
use web_sys::*;
use yew_websocket::websocket::{Binary, Text};

pub mod decode;

pub struct MediaPacketWrapper(pub MediaPacket);

impl From<Text> for MediaPacketWrapper {
    fn from(_: Text) -> Self {
        MediaPacketWrapper(MediaPacket::default())
    }
}

impl From<Binary> for MediaPacketWrapper {
    fn from(bin: Binary) -> Self {
        let media_packet: MediaPacket = bin
            .map(|data| MediaPacket::parse_from_bytes(&data.into_boxed_slice()).unwrap())
            .unwrap_or_default();
        MediaPacketWrapper(media_packet)
    }
}

pub struct EncodedVideoChunkTypeWrapper(pub EncodedVideoChunkType);

impl From<&str> for EncodedVideoChunkTypeWrapper {
    fn from(s: &str) -> Self {
        match s {
            "key" => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Key),
            _ => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedVideoChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedVideoChunkType::Delta => write!(f, "delta"),
            EncodedVideoChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
}

pub struct EncodedAudioChunkTypeWrapper(pub EncodedAudioChunkType);

impl From<String> for EncodedAudioChunkTypeWrapper {
    fn from(s: String) -> Self {
        match s.as_str() {
            "key" => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key),
            _ => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedAudioChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedAudioChunkType::Delta => write!(f, "delta"),
            EncodedAudioChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
}

pub struct AudioSampleFormatWrapper(pub AudioSampleFormat);

impl From<String> for AudioSampleFormatWrapper {
    fn from(s: String) -> Self {
        match s.as_str() {
            "u8" => AudioSampleFormatWrapper(AudioSampleFormat::U8),
            "s16" => AudioSampleFormatWrapper(AudioSampleFormat::S16),
            "s32" => AudioSampleFormatWrapper(AudioSampleFormat::S32),
            "f32" => AudioSampleFormatWrapper(AudioSampleFormat::F32),
            "u8-planar" => AudioSampleFormatWrapper(AudioSampleFormat::U8Planar),
            "s16-planar" => AudioSampleFormatWrapper(AudioSampleFormat::S16Planar),
            "s32-planar" => AudioSampleFormatWrapper(AudioSampleFormat::S32Planar),
            "f32-planar" => AudioSampleFormatWrapper(AudioSampleFormat::F32Planar),
            _ => todo!(),
        }
    }
}

impl fmt::Display for AudioSampleFormatWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            AudioSampleFormat::U8 => write!(f, "u8"),
            AudioSampleFormat::S16 => write!(f, "s16"),
            AudioSampleFormat::S32 => write!(f, "s32"),
            AudioSampleFormat::F32 => write!(f, "f32"),
            AudioSampleFormat::U8Planar => write!(f, "u8-planar"),
            AudioSampleFormat::S16Planar => write!(f, "s16-planar"),
            AudioSampleFormat::S32Planar => write!(f, "s32-planar"),
            AudioSampleFormat::F32Planar => write!(f, "f32-planar"),
            _ => todo!(),
        }
    }
}

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
) -> MediaPacket {
    chunk.copy_to_with_u8_array(buffer);
    let mut packet: MediaPacket = MediaPacket::default();
    packet.email = email.clone();
    packet.media_type = MediaType::AUDIO.into();
    packet.data = buffer[0..chunk.byte_length() as usize].to_vec();
    packet.frame_type = EncodedAudioChunkTypeWrapper(chunk.type_()).to_string();
    packet.timestamp = chunk.timestamp();
    if let Some(duration0) = chunk.duration() {
        packet.duration = duration0;
    }
    packet
}
