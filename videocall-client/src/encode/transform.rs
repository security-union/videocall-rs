use super::super::wrappers::{EncodedAudioChunkTypeWrapper, EncodedVideoChunkTypeWrapper};
use crate::crypto::aes::Aes128State;
use crate::client::diagnostics::DiagnosticsManager;
use protobuf::Message;
use std::rc::Rc;
use videocall_types::protos::{
    media_packet::{media_packet::MediaType, MediaPacket, VideoMetadata, AudioMetadata},
    packet_wrapper::{packet_wrapper::PacketType, PacketWrapper},
};
use videocall_types::protos::media_packet::media_packet::MediaType as MediaPacketType;
use web_sys::{EncodedAudioChunk, EncodedVideoChunk};

pub fn transform_video_chunk(
    chunk: EncodedVideoChunk,
    sequence: u64,
    buffer: &mut [u8],
    email: &str,
    aes: Rc<Aes128State>,
    diag_manager: Option<&mut DiagnosticsManager>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    
    #[cfg(target_arch = "wasm32")]
    let now = js_sys::Date::now();
    
    #[cfg(not(target_arch = "wasm32"))]
    let now = 1000.0; // Mock timestamp for non-WASM environments
    
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
    
    // Update diagnostics if available
    if let Some(diag) = diag_manager {
        // Record that we're encoding frame with this sequence number
        diag.on_frame_encoded(
            email, 
            MediaPacketType::VIDEO, 
            now, 
            byte_length as u32,
            sequence
        );
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
    diag_manager: Option<&mut DiagnosticsManager>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    
    #[cfg(target_arch = "wasm32")]
    let now = js_sys::Date::now();
    
    #[cfg(not(target_arch = "wasm32"))]
    let now = 1000.0; // Mock timestamp for non-WASM environments
    
    let mut media_packet: MediaPacket = MediaPacket {
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedVideoChunkTypeWrapper(chunk.type_()).to_string(),
        email: email.to_owned(),
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
    
    // Update diagnostics if available
    if let Some(diag) = diag_manager {
        // Record that we're encoding frame with this sequence number
        diag.on_frame_encoded(
            email, 
            MediaPacketType::SCREEN, 
            now, 
            byte_length as u32,
            sequence
        );
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
    diag_manager: Option<&mut DiagnosticsManager>,
) -> PacketWrapper {
    let byte_length = chunk.byte_length() as usize;
    chunk.copy_to_with_u8_array(buffer);
    
    #[cfg(target_arch = "wasm32")]
    let now = js_sys::Date::now();
    
    #[cfg(not(target_arch = "wasm32"))]
    let now = 1000.0; // Mock timestamp for non-WASM environments
    
    let mut media_packet = MediaPacket {
        data: buffer[0..byte_length].to_vec(),
        frame_type: EncodedAudioChunkTypeWrapper(chunk.type_()).to_string(),
        email: email.to_owned(),
        media_type: MediaType::AUDIO.into(),
        timestamp: chunk.timestamp(),
        audio_metadata: Some(AudioMetadata {
            audio_sample_rate: 0.0,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    
    if let Some(duration0) = chunk.duration() {
        media_packet.duration = duration0;
    }
    
    // Update diagnostics if available
    if let Some(diag) = diag_manager {
        // Record that we're encoding frame with this sequence number
        diag.on_frame_encoded(
            email, 
            MediaPacketType::AUDIO, 
            now, 
            byte_length as u32,
            sequence
        );
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
