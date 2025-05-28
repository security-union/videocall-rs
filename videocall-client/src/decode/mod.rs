pub mod audio_decoder_wrapper;
pub mod config;
pub mod hash_map_with_ordered_keys;
pub mod media_decoder_trait;
pub mod media_decoder_with_buffer;
pub mod peer_decode_manager;
pub mod peer_decoder;
pub mod safari;
pub mod video_decoder_wrapper;

pub use media_decoder_with_buffer::{
    AudioDecoderWithBuffer, MediaDecoderWithBuffer, VideoDecoderWithBuffer,
};
pub use peer_decode_manager::{PeerDecodeManager, PeerStatus};
pub use peer_decoder::VideoPeerDecoder;

use crate::utils::is_ios;
use peer_decoder::{
    DecodeStatus as StandardDecodeStatus, PeerDecode as StandardPeerDecodeTrait,
    StandardAudioPeerDecoder,
};
use safari::audio_decoder::{
    AudioPeerDecoder as SafariAudioPeerDecoder, DecodeStatus as SafariDecodeStatus,
    PeerDecode as SafariPeerDecodeTrait,
};
use std::sync::Arc;
use videocall_types::protos::media_packet::MediaPacket;
use wasm_bindgen::JsValue;

// Common DecodeStatus for the trait
#[derive(Debug, Clone, Copy)]
pub struct DecodeStatus {
    pub rendered: bool,
    pub first_frame: bool,
}

impl From<StandardDecodeStatus> for DecodeStatus {
    fn from(status: StandardDecodeStatus) -> Self {
        DecodeStatus {
            rendered: status._rendered, // Note the underscore, matches StandardAudioPeerDecoder's field
            first_frame: status.first_frame,
        }
    }
}

impl From<SafariDecodeStatus> for DecodeStatus {
    fn from(status: SafariDecodeStatus) -> Self {
        DecodeStatus {
            rendered: status.rendered, // Matches SafariAudioPeerDecoder's field
            first_frame: status.first_frame,
        }
    }
}

/// Trait to abstract over different audio peer decoder implementations
pub trait AudioPeerDecoderTrait {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus>;
}

// Implement trait for standard audio peer decoder
impl AudioPeerDecoderTrait for StandardAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        StandardPeerDecodeTrait::decode(self, packet).map(|status| status.into())
    }
}

// Implement trait for Safari audio peer decoder
impl AudioPeerDecoderTrait for SafariAudioPeerDecoder {
    fn decode(&mut self, packet: &Arc<MediaPacket>) -> anyhow::Result<DecodeStatus> {
        SafariPeerDecodeTrait::decode(self, packet)
            .map_err(|_| anyhow::anyhow!("Safari audio decoder failed"))
            .map(|status| status.into())
    }
}

/// Factory function to create the appropriate audio peer decoder based on platform detection
pub fn create_audio_peer_decoder(
    speaker_device_id: Option<String>,
) -> Result<Box<dyn AudioPeerDecoderTrait>, JsValue> {
    if is_ios() {
        log::info!(
            "Platform detection: Using Safari (AudioWorklet) audio peer decoder for iOS device"
        );
        Ok(Box::new(SafariAudioPeerDecoder::new_with_speaker(
            speaker_device_id,
        )))
    } else {
        log::info!("Platform detection: Using standard (AudioDecoder API) audio peer decoder");
        StandardAudioPeerDecoder::new(speaker_device_id)
            .map(|decoder| Box::new(decoder) as Box<dyn AudioPeerDecoderTrait>)
    }
}

// No need to re-export PeerDecode or VideoPeerDecodeTrait here as they are specific to their implementations.
// VideoPeerDecoder is re-exported above for direct use.
