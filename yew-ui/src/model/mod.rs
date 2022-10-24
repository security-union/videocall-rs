use protobuf::Message;
use std::fmt;
use types::protos::rust::media_packet::{
    media_packet::{self, MediaType},
    MediaPacket,
};
use web_sys::*;
use yew_websocket::websocket::{Binary, Text};
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
            .unwrap_or(MediaPacket::default());
        MediaPacketWrapper(media_packet)
    }
}

pub struct EncodedVideoChunkTypeWrapper(pub EncodedVideoChunkType);

impl From<String> for EncodedVideoChunkTypeWrapper {
    fn from(s: String) -> Self {
        match s.as_str() {
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
    buffer: &mut [u8],
    email: Box<String>,
) -> MediaPacket {
    let mut media_packet: MediaPacket = MediaPacket::default();
    media_packet.email = *email.clone();
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    media_packet.video = buffer[0..byte_length].to_vec();
    media_packet.video_type = EncodedVideoChunkTypeWrapper(chunk.type_()).to_string();
    media_packet.media_type = media_packet::MediaType::VIDEO.into();
    media_packet.timestamp = chunk.timestamp();
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    media_packet
}

pub fn transform_audio_chunk(
    audio_frame: &AudioData,
    buffer: &mut [u8],
    email: &String,
) -> MediaPacket {
    let byte_length: usize = audio_frame.allocation_size(&AudioDataCopyToOptions::new(0)) as usize;
    audio_frame.copy_to_with_u8_array(buffer, &AudioDataCopyToOptions::new(0));
    let mut packet: MediaPacket = MediaPacket::default();
    packet.email = email.clone();
    packet.media_type = MediaType::AUDIO.into();
    packet.audio = buffer[0..byte_length].to_vec();
    packet.audio_format = AudioSampleFormatWrapper(audio_frame.format().unwrap()).to_string();
    packet.audio_number_of_channels = audio_frame.number_of_channels();
    packet.audio_number_of_frames = audio_frame.number_of_frames();
    packet.audio_sample_rate = audio_frame.sample_rate();
    packet
}
